//! Explicit post-restore selection. Configuration is published as one file only
//! after both storage components and runtime access have been revalidated.
use super::{
    deployment_backup,
    deployment_restore::Receipt,
    runtime::{ManagedCluster, Status},
};
use crate::{
    config::{
        database::{self, DatabaseTarget, DeploymentConfig, Environment, UserSettings},
        switch::{Outcome, Publication},
    },
    knowledge::{document::digest, store::KnowledgeBackup},
};
use anyhow::{Context, Result, ensure};
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

const OVERRIDES: &[&str] = &[
    "YGG_DB_MODE",
    "DATABASE_URL",
    "YGG_DATABASE_OWNER_URL",
    "YGG_DATA_DIR",
    "YGG_PROFILE",
    "YGG_KNOWLEDGE_DIR",
    "YGG_KNOWLEDGE_POLICY_DIR",
];
fn private_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "switch input must be a private owned regular file"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "switch input exceeds size limit"
    );
    Ok(bytes)
}
fn same(a: &DeploymentConfig, b: &DeploymentConfig) -> bool {
    a.database == b.database
        && a.data_dir == b.data_dir
        && a.knowledge_dir == b.knowledge_dir
        && a.knowledge_policy_dir == b.knowledge_policy_dir
        && a.owner_url.as_ref().map(|u| u.as_str()) == b.owner_url.as_ref().map(|u| u.as_str())
}
async fn runtime_access(options: &PgConnectOptions) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection=PgConnection::connect_with(options).await.map_err(|_|anyhow::anyhow!("proposed runtime connection failed"))?;
        let allowed:bool=sqlx::query_scalar("SELECT bool_and(has_table_privilege(current_user, 'public.' || t.name, p.privilege)) FROM unnest($1::text[]) t(name) CROSS JOIN unnest(ARRAY['SELECT','INSERT','UPDATE','DELETE']) p(privilege)").bind(super::provision::TABLES).fetch_one(&mut connection).await?;
        ensure!(allowed,"proposed runtime lacks application CRUD grants; configure target grants before switching");
        let markers:bool=sqlx::query_scalar("SELECT has_schema_privilege(current_user,'public','USAGE') AND has_table_privilege(current_user,'public.knowledge_storage','SELECT') AND has_table_privilege(current_user,'public._sqlx_migrations','SELECT')").fetch_one(&mut connection).await?;
        ensure!(markers,"proposed runtime cannot read storage/migration markers");
        connection.close().await?;Ok(())
    }).await.map_err(|_|anyhow::anyhow!("proposed runtime validation timed out"))?
}

pub async fn run(
    backup: &Path,
    restored: &Path,
    target_config: &Path,
    env: Environment,
    resume: Option<Uuid>,
) -> Result<Outcome> {
    let manifest = deployment_backup::verify(backup)?;
    let restored = restored.canonicalize()?;
    let receipt: Receipt = serde_json::from_slice(&private_bytes(&restored.join("restore.json"))?)?;
    ensure!(
        receipt.version == 1
            && !receipt.configuration_switched
            && receipt.database_id == manifest.database.database_id
            && receipt.generation == manifest.database.generation
            && receipt.source_dump_sha256 == manifest.database.sha256
            && receipt.source_configuration_sha256
                == manifest.configuration.as_ref().map(|c| c.sha256.clone())
            && receipt.corpus_id == manifest.knowledge.as_ref().map(|k| k.corpus_id)
            && receipt.destination.canonicalize()? == restored,
        "restore receipt does not match backup/destination"
    );
    if let Some(configuration) = &manifest.configuration {
        deployment_backup::configuration_bytes(
            &restored.join("source-configuration.json"),
            configuration,
        )?;
    }
    let next = private_bytes(target_config)?;
    let settings: UserSettings = toml::from_str(
        std::str::from_utf8(&next)
            .map_err(|_| anyhow::anyhow!("target configuration must be UTF-8"))?,
    )
    .map_err(|_| anyhow::anyhow!("invalid target configuration"))?;
    ensure!(
        settings.database.mode.is_some()
            && settings.data_dir.is_some()
            && settings.knowledge_dir.is_some()
            && settings.knowledge_policy_dir.is_some(),
        "target configuration must explicitly select database mode, data_dir, knowledge_dir and knowledge_policy_dir"
    );
    let mut clean = env.clone();
    for key in OVERRIDES {
        clean.remove(*key);
    }
    let target = DeploymentConfig::resolve(&settings, &clean)?;
    let effective = database::user_environment(env.clone())?;
    ensure!(
        same(&target, &DeploymentConfig::resolve(&settings, &effective)?),
        "environment or user .env overrides the proposed deployment; remove conflicting overrides before switching"
    );
    if manifest.knowledge.is_some() {
        ensure!(
            receipt
                .knowledge_dir
                .as_ref()
                .is_some_and(|p| p == &restored.join("knowledge"))
                && receipt
                    .policy_dir
                    .as_ref()
                    .is_some_and(|p| p == &restored.join("policy")),
            "unexpected restored corpus/policy paths"
        );
        ensure!(
            target.knowledge_dir.canonicalize()? == restored.join("knowledge")
                && target.knowledge_policy_dir.canonicalize()? == restored.join("policy"),
            "proposed configuration must select restored corpus and policy together"
        );
    } else {
        ensure!(
            receipt.knowledge_dir.is_none() && receipt.policy_dir.is_none(),
            "unexpected corpus in SQL-only restore receipt"
        );
        ensure!(
            std::fs::symlink_metadata(&target.knowledge_dir)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                && std::fs::symlink_metadata(&target.knowledge_policy_dir)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "SQL-only backup cannot select existing unrelated knowledge or policy"
        );
    }
    let root = database::config_dir(&env)?;
    let binding = digest(&serde_json::to_vec(&(
        backup.canonicalize()?,
        &restored,
        digest(&serde_json::to_vec(&manifest)?),
    ))?);
    let publication = Publication::begin(&root, &next, &binding, resume)?;
    let operation = publication.operation();
    let result = async {
        if publication.already_applied()? {
            return Ok(publication.outcome(true));
        }
        if manifest.knowledge.is_some() {
            KnowledgeBackup::verify_restored(&backup.join("knowledge"), &target.knowledge_dir)?;
            KnowledgeBackup::verify_restored(&backup.join("policy"), &target.knowledge_policy_dir)?;
        }
        let (owner, runtime, started) = match &target.database {
            DatabaseTarget::External { url } => {
                let owner = target.owner_url.as_ref().map(|u| u.as_str()).unwrap_or(url);
                super::external::validate_owner_target(url, owner)?;
                (
                    super::external::options(owner)?,
                    super::external::options(url)?,
                    None,
                )
            }
            DatabaseTarget::ManagedLocal { data_dir } => {
                let cluster = ManagedCluster::open(&data_dir.join("postgres"))?;
                let was_stopped = matches!(cluster.status().await?, Status::Stopped);
                super::supervisor::start(
                    &cluster,
                    &std::env::current_exe()?,
                    Duration::from_secs(30),
                )
                .await?;
                (
                    cluster
                        .admin_options()
                        .username("ygg_owner")
                        .database("ygg"),
                    super::provision::runtime_options(&cluster),
                    was_stopped.then_some(cluster),
                )
            }
        };
        let checked = async {
            super::restore::validate(&owner, &manifest.database).await?;
            runtime_access(&runtime).await?;
            // Reread filesystem/config inputs after the database scan. Changes
            // refuse publication rather than silently selecting a mixed revision.
            ensure!(
                private_bytes(target_config)? == next,
                "target configuration changed during validation"
            );
            let effective = database::user_environment(env.clone())?;
            ensure!(
                same(&target, &DeploymentConfig::resolve(&settings, &effective)?),
                "user .env changed during validation"
            );
            if manifest.knowledge.is_some() {
                KnowledgeBackup::verify_restored(&backup.join("knowledge"), &target.knowledge_dir)?;
                KnowledgeBackup::verify_restored(
                    &backup.join("policy"),
                    &target.knowledge_policy_dir,
                )?;
            }
            ensure!(
                deployment_backup::verify(backup)? == manifest,
                "backup changed during switch validation"
            );
            publication.commit()
        }
        .await;
        if checked.is_err() {
            if let Some(cluster) = started {
                let _ = super::supervisor::stop(&cluster, Duration::from_secs(30)).await;
            }
        }
        checked
    }
    .await;
    result.with_context(|| {
        format!(
            "deployment switch {operation}; journal retained in {}",
            root.display()
        )
    })
}
