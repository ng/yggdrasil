//! Explicit combined backup publication. PostgreSQL and filesystem revisions are
//! recorded separately; quiesce writers for a maintenance/deployment move.
use super::{
    backup::{self, DatabaseSnapshot},
    runtime::{ManagedCluster, Status},
};
use crate::{
    config::database::{DatabaseTarget, DeploymentConfig},
    knowledge::{
        identity::IdentityRegistry,
        store::{KnowledgeBackup, KnowledgeStore},
    },
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeRevision {
    pub corpus_id: Uuid,
    pub bundle: String,
    pub policy: String,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub database: DatabaseSnapshot,
    pub knowledge: Option<KnowledgeRevision>,
}

fn private_root(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "backup root must be a private owned directory"
    );
    Ok(())
}
fn file(path: &Path, create: bool) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(create)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?)
}
fn names(path: &Path) -> Result<Vec<String>> {
    let mut names = std::fs::read_dir(path)?
        .take(5)
        .map(|entry| {
            let name = entry?.file_name();
            name.into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 backup entry"))
        })
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}
fn checksum(file: &mut File) -> Result<(u64, String)> {
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
        "dump must be a regular, non-hardlinked file"
    );
    let mut header = [0u8; 5];
    file.read_exact(&mut header)?;
    ensure!(
        &header == b"PGDMP",
        "database backup is not a custom-format archive"
    );
    let mut hash = Sha256::new();
    hash.update(header);
    let mut bytes = 5;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        hash.update(&buffer[..count]);
    }
    Ok((
        bytes,
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    ))
}

/// Offline integrity verification, not a replacement for a restore rehearsal.
pub fn verify(path: &Path) -> Result<Manifest> {
    private_root(path)?;
    let mut encoded = Vec::new();
    file(&path.join("backup.json"), false)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut encoded)?;
    ensure!(
        encoded.len() <= 4 * 1024 * 1024,
        "backup manifest exceeds size limit"
    );
    let manifest: Manifest = serde_json::from_slice(&encoded)?;
    ensure!(
        manifest.version == 1,
        "unsupported deployment backup version"
    );
    ensure!(
        manifest.database.generation > 0
            && matches!(manifest.database.backend.as_str(), "sql" | "fenced" | "okf")
            && (manifest.database.backend == "sql") == manifest.database.corpus_id.is_none(),
        "invalid database storage binding in backup"
    );
    let expected = if manifest.knowledge.is_some() {
        vec!["backup.json", "database.dump", "knowledge", "policy"]
    } else {
        vec!["backup.json", "database.dump"]
    };
    ensure!(
        names(path)? == expected,
        "unexpected deployment backup contents"
    );
    let actual = checksum(&mut file(&path.join("database.dump"), false)?)?;
    ensure!(
        actual == (manifest.database.bytes, manifest.database.sha256.clone()),
        "database backup integrity mismatch"
    );
    if let Some(knowledge) = &manifest.knowledge {
        let bundle = KnowledgeBackup::verify(&path.join("knowledge"))?;
        let policy = KnowledgeBackup::verify(&path.join("policy"))?;
        let identity = IdentityRegistry::open(&path.join("policy/corpus"), false)?
            .read()?
            .0;
        ensure!(
            bundle.revision == knowledge.bundle
                && policy.revision == knowledge.policy
                && identity.corpus_id == knowledge.corpus_id,
            "knowledge backup revision or identity mismatch"
        );
        ensure!(
            manifest
                .database
                .corpus_id
                .is_none_or(|id| id == knowledge.corpus_id),
            "database and knowledge corpus bindings differ"
        );
    } else {
        ensure!(
            manifest.database.backend == "sql" && manifest.database.corpus_id.is_none(),
            "file-backed database requires knowledge and policy backups"
        );
    }
    Ok(manifest)
}

pub async fn create(
    config: &DeploymentConfig,
    destination: &Path,
    pg_bin: Option<&Path>,
    policy_dir: Option<&Path>,
) -> Result<Manifest> {
    ensure!(
        destination.is_absolute(),
        "absolute backup destination required"
    );
    match std::fs::symlink_metadata(destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => anyhow::bail!("backup destination already exists"),
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("backup parent required"))?
        .canonicalize()?;
    let parent_meta = std::fs::metadata(&parent)?;
    ensure!(
        parent_meta.uid() == unsafe { libc::geteuid() } && parent_meta.mode() & 0o022 == 0,
        "backup parent must be owned and not writable by other users"
    );
    let destination = parent.join(
        destination
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("backup name required"))?,
    );
    let knowledge = match std::fs::symlink_metadata(&config.knowledge_dir) {
        Ok(_) => {
            let bundle = KnowledgeStore::open(&config.knowledge_dir, false)?;
            let policy_path = policy_dir.unwrap_or(&config.knowledge_policy_dir);
            let policy = KnowledgeStore::open(policy_path, false)?;
            let a = config.knowledge_dir.canonicalize()?;
            let b = policy_path.canonicalize()?;
            ensure!(
                !a.starts_with(&b) && !b.starts_with(&a),
                "knowledge and policy directories must not overlap"
            );
            ensure!(
                !parent.starts_with(&a) && !parent.starts_with(&b),
                "backup destination cannot be inside knowledge or policy"
            );
            Some((bundle, policy))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                policy_dir.is_none(),
                "policy supplied but knowledge directory is absent"
            );
            ensure!(
                std::fs::symlink_metadata(&config.knowledge_policy_dir)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "policy exists or is inaccessible but knowledge directory is absent; refusing to omit policy from backup"
            );
            None
        }
        Err(error) => return Err(error.into()),
    };
    let (bin, options) = match &config.database {
        DatabaseTarget::External { url } => {
            let selected = config
                .owner_url
                .as_ref()
                .map(|owner| owner.as_str())
                .unwrap_or(url);
            super::external::validate_owner_target(url, selected)?;
            (
                pg_bin
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "external backup requires --pg-bin with compatible native tools"
                        )
                    })?
                    .to_owned(),
                super::external::options(selected)?,
            )
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            ensure!(
                pg_bin.is_none(),
                "managed backup uses its pinned tools; --pg-bin is external-only"
            );
            let cluster = ManagedCluster::open(&data_dir.join("postgres"))?;
            ensure!(
                matches!(cluster.status().await?, Status::Ready { .. }),
                "managed database must be running; backup does not start it"
            );
            (
                cluster.bin().to_owned(),
                cluster
                    .admin_options()
                    .username("ygg_owner")
                    .database("ygg"),
            )
        }
    };
    ensure!(
        bin.is_absolute(),
        "absolute PostgreSQL binary directory required"
    );
    // Validate native TLS/credentials before creating any output state.
    backup::NativeConnection::from_options(&options)?;
    let stage = parent.join(format!(".deployment-backup-{}", Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&stage)?;
    File::open(&parent)?.sync_all()?;
    let database = backup::dump(
        &bin,
        &options,
        &mut file(&stage.join("database.dump"), true)?,
    )
    .await?;
    let knowledge = match knowledge {
        Some((bundle, policy)) => {
            let (bundle, policy) =
                bundle.backup_pair(&policy, &stage.join("knowledge"), &stage.join("policy"))?;
            let identity = IdentityRegistry::open(&stage.join("policy/corpus"), false)?
                .read()?
                .0;
            Some(KnowledgeRevision {
                corpus_id: identity.corpus_id,
                bundle: bundle.revision,
                policy: policy.revision,
            })
        }
        None => None,
    };
    // Refuse a storage transition occurring between the database and file snapshots.
    let mut current = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        PgConnection::connect_with(&options),
    )
    .await
    .map_err(|_| anyhow::anyhow!("backup source revalidation timed out"))?
    .map_err(|_| anyhow::anyhow!("cannot revalidate backup source"))?;
    let marker: (Uuid, i64, String, Option<Uuid>) = sqlx::query_as("SELECT database_id, generation, backend, corpus_id FROM public.knowledge_storage WHERE singleton").fetch_one(&mut current).await?;
    current.close().await?;
    ensure!(
        marker
            == (
                database.database_id,
                database.generation,
                database.backend.clone(),
                database.corpus_id
            ),
        "database storage generation changed during backup"
    );
    let manifest = Manifest {
        version: 1,
        created_at: chrono::Utc::now(),
        database,
        knowledge,
    };
    let encoded = serde_json::to_vec_pretty(&manifest)?;
    ensure!(
        encoded.len() <= 4 * 1024 * 1024,
        "backup manifest exceeds size limit"
    );
    let mut output = file(&stage.join("backup.json"), true)?;
    output.write_all(&encoded)?;
    output.sync_all()?;
    File::open(&stage)?.sync_all()?;
    ensure!(
        verify(&stage)? == manifest,
        "staged backup verification failed"
    );
    super::package::publish(&stage, &destination)?;
    File::open(&parent)?.sync_all()?;
    Ok(manifest)
}
