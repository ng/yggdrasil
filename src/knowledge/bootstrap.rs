//! First-run OKF initialization. Creates the private corpus and policy
//! directories, mints the identity policy and the local selection binding, and
//! binds the selected database's storage marker to this corpus. Idempotent:
//! an existing selection is verified and left untouched.
use super::{
    identity::IdentityRegistry,
    legacy::Mappings,
    runtime::{Binding, Phase, SELECTION_FILE, configured_user},
    store::KnowledgeStore,
};
use crate::config::database::{DeploymentConfig, Environment};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::Serialize;
use sqlx::PgPool;
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Report {
    pub created: bool,
    pub corpus_id: Uuid,
    pub database_id: Uuid,
    pub generation: i64,
    pub user: String,
    pub knowledge_dir: PathBuf,
    pub knowledge_policy_dir: PathBuf,
}

/// The pool must be able to read `knowledge_storage` and, on a fresh database,
/// advance it (migration owner). Ordinary runtime pools can only verify.
pub async fn initialize(
    config: &DeploymentConfig,
    env: &Environment,
    pool: &PgPool,
) -> Result<Report> {
    let user = configured_user(env);
    ensure!(!user.trim().is_empty(), "knowledge user identity required");
    ensure!(
        !config
            .knowledge_dir
            .starts_with(&config.knowledge_policy_dir)
            && !config
                .knowledge_policy_dir
                .starts_with(&config.knowledge_dir),
        "knowledge and policy directories must be separate"
    );
    KnowledgeStore::open(&config.knowledge_dir, true)
        .with_context(|| format!("creating {}", config.knowledge_dir.display()))?;
    let registry = IdentityRegistry::open(&config.knowledge_policy_dir, true)
        .with_context(|| format!("creating {}", config.knowledge_policy_dir.display()))?;
    let identities = registry.initialize(true)?;
    let policy = KnowledgeStore::open(&config.knowledge_policy_dir, false)?;
    let bundle = config.knowledge_dir.canonicalize()?;
    if let Some(existing) = policy.read_control(SELECTION_FILE)? {
        let binding: Binding = serde_json::from_str(&existing)
            .context("existing knowledge selection is unreadable; restore policy from backup")?;
        ensure!(
            binding.mappings.corpus_id == identities.corpus_id,
            "existing knowledge selection names another corpus"
        );
        ensure!(
            binding.phase == Phase::Okf,
            "knowledge cutover is fenced; resume or abort it with the release that started it"
        );
        // The local selection alone proves nothing about the connected database:
        // a different or restored DATABASE_URL must be reported here, not as a
        // later lease failure on every claim.
        let (database_id, generation, backend, bound): (Uuid, i64, String, Option<Uuid>) =
            sqlx::query_as(
                "SELECT database_id, generation, backend, corpus_id \
                 FROM public.knowledge_storage WHERE singleton",
            )
            .fetch_one(pool)
            .await?;
        ensure!(
            database_id == binding.mappings.database_id
                && generation == binding.generation
                && backend == "okf"
                && bound == Some(identities.corpus_id),
            "existing knowledge selection is bound to a different database or generation \
             (selected {}@{}, connected {}@{} backend {backend})",
            binding.mappings.database_id,
            binding.generation,
            database_id,
            generation
        );
        return Ok(Report {
            created: false,
            corpus_id: identities.corpus_id,
            database_id: binding.mappings.database_id,
            generation: binding.generation,
            user,
            knowledge_dir: bundle,
            knowledge_policy_dir: config.knowledge_policy_dir.clone(),
        });
    }
    let (database_id, generation) = bind_marker(pool, identities.corpus_id).await?;
    let binding = Binding {
        version: 1,
        minimum_client: super::guard::CLIENT_PROTOCOL,
        generation,
        phase: Phase::Okf,
        bundle: bundle.clone(),
        mappings: Mappings {
            database_id,
            corpus_id: identities.corpus_id,
            repos: BTreeMap::new(),
            users: BTreeMap::from([(user.clone(), user.clone())]),
        },
        agents: BTreeMap::new(),
    };
    policy.update_control(SELECTION_FILE, |current| {
        ensure!(
            current.is_none(),
            "knowledge selection appeared concurrently; rerun"
        );
        Ok((serde_json::to_string_pretty(&binding)?, ()))
    })?;
    Ok(Report {
        created: true,
        corpus_id: identities.corpus_id,
        database_id,
        generation,
        user,
        knowledge_dir: bundle,
        knowledge_policy_dir: config.knowledge_policy_dir.clone(),
    })
}

/// Bind the database marker to this corpus. A fresh database starts at `sql`
/// and must pass through `fenced` (trigger contract); a database already bound
/// to this corpus is accepted; one bound to another corpus is refused.
async fn bind_marker(pool: &PgPool, corpus: Uuid) -> Result<(Uuid, i64)> {
    let mut tx = pool.begin().await?;
    let (database_id, generation, backend, bound): (Uuid, i64, String, Option<Uuid>) =
        sqlx::query_as(
            "SELECT database_id, generation, backend, corpus_id \
             FROM public.knowledge_storage WHERE singleton FOR UPDATE",
        )
        .fetch_one(&mut *tx)
        .await?;
    match backend.as_str() {
        "okf" => {
            ensure!(
                bound == Some(corpus),
                "database is bound to knowledge corpus {}; restore that corpus or use its policy directory",
                bound
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "<unknown>".into())
            );
            tx.rollback().await?;
            Ok((database_id, generation))
        }
        "fenced" => bail!("knowledge cutover is in progress on this database"),
        "sql" => {
            // The marker trigger requires sql -> fenced -> okf, one generation
            // per step, and the check constraint requires a corpus once fenced.
            for next_backend in ["fenced", "okf"] {
                sqlx::query(
                    "UPDATE public.knowledge_storage \
                     SET generation = generation + 1, backend = $1, corpus_id = $2 \
                     WHERE singleton",
                )
                .bind(next_backend)
                .bind(corpus)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    anyhow!(
                        "binding knowledge marker failed (migration-owner rights required): {e}"
                    )
                })?;
            }
            tx.commit().await?;
            Ok((database_id, generation + 2))
        }
        other => bail!("unknown knowledge storage backend {other}"),
    }
}
