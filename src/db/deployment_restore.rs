//! Explicit recovery into a new destination; never switch configuration here.
use super::{backup::NativeConnection, deployment_backup, runtime::ManagedCluster};
use crate::{
    config::database::{DatabaseTarget, DeploymentConfig},
    knowledge::store::KnowledgeBackup,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub restored_at: chrono::DateTime<chrono::Utc>,
    pub database_id: Uuid,
    pub generation: i64,
    pub corpus_id: Option<Uuid>,
    pub source_dump_sha256: String,
    pub destination: PathBuf,
    pub knowledge_dir: Option<PathBuf>,
    pub policy_dir: Option<PathBuf>,
    pub configuration_switched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_rebase: Option<crate::knowledge::relocation::SelectionRebase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_configuration_sha256: Option<String>,
}

fn absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => anyhow::bail!("restore destination already exists: {}", path.display()),
    }
}

pub async fn run(
    config: &DeploymentConfig,
    source: &Path,
    destination: &Path,
    pg_bin: Option<&Path>,
    postgres_archive: Option<&Path>,
) -> Result<Receipt> {
    let manifest = deployment_backup::verify(source)?;
    ensure!(
        manifest
            .database
            .validation
            .as_ref()
            .is_some_and(|v| v.version == 1),
        "backup lacks supported restore evidence; create a new backup"
    );
    ensure!(
        destination.is_absolute(),
        "absolute restore destination required"
    );
    absent(destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("restore parent required"))?
        .canonicalize()?;
    let metadata = std::fs::metadata(&parent)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
        "restore parent must be owned and not writable by others"
    );
    ensure!(
        !parent.starts_with(source.canonicalize()?),
        "restore destination cannot be inside backup"
    );
    let destination = parent.join(
        destination
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("restore name required"))?,
    );
    // Resolve credentials and reject invalid combinations before any creation.
    let external = match &config.database {
        DatabaseTarget::External { url } => {
            ensure!(
                postgres_archive.is_none(),
                "--postgres-archive is managed-only"
            );
            let bin =
                pg_bin.ok_or_else(|| anyhow::anyhow!("external restore requires --pg-bin"))?;
            ensure!(
                bin.is_absolute(),
                "absolute PostgreSQL binary directory required"
            );
            let selected = config.owner_url.as_ref().map(|u| u.as_str()).unwrap_or(url);
            super::external::validate_owner_target(url, selected)?;
            let options = super::external::options(selected)?;
            NativeConnection::from_options(&options)?;
            Some((bin, options))
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            ensure!(pg_bin.is_none(), "managed restore uses its pinned tools");
            ensure!(
                manifest.database.server_major <= 16,
                "managed PostgreSQL 16 cannot restore a newer major"
            );
            ensure!(
                !data_dir
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("managed data parent required"))?
                    .canonicalize()?
                    .starts_with(source.canonicalize()?),
                "managed data cannot be inside backup"
            );
            absent(data_dir).context(
                "managed restore requires a new YGG_DATA_DIR; existing deployment retained",
            )?;
            ensure!(
                !destination.starts_with(data_dir) && !data_dir.starts_with(&destination),
                "managed data and restored files must be separate directories"
            );
            None
        }
    };
    let selection = if manifest.knowledge.is_some() {
        crate::knowledge::relocation::prepare(
            &source.join("policy"),
            &destination.join("knowledge"),
            &manifest.database,
        )?
    } else {
        None
    };
    let stage = parent.join(format!(".deployment-restore-{}", Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&stage)?;
    File::open(&parent)?.sync_all()?;
    if manifest.knowledge.is_some() {
        KnowledgeBackup::restore(&source.join("knowledge"), &stage.join("knowledge"))?;
        KnowledgeBackup::restore(&source.join("policy"), &stage.join("policy"))?;
        if let Some(selection) = &selection {
            selection.apply(&stage.join("policy"))?;
            selection.verify(&source.join("policy"), &stage.join("policy"))?;
        }
    }
    if let Some(configuration) = &manifest.configuration {
        let bytes = deployment_backup::configuration_bytes(
            &source.join("configuration.json"),
            configuration,
        )?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(stage.join("source-configuration.json"))?;
        output.write_all(&bytes)?;
        output.sync_all()?;
    }
    // Use the open archive descriptor for verification and native input. Source
    // replacement cannot redirect the child to a different path.
    let mut archive = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source.join("database.dump"))?;
    match external {
        Some((bin, options)) => {
            super::restore::database(bin, &options, &mut archive, &manifest.database).await?
        }
        None => {
            let DatabaseTarget::ManagedLocal { data_dir } = &config.database else {
                unreachable!()
            };
            // Reserve the new target exclusively; a racing initializer must not
            // cause us to adopt an already existing deployment.
            std::fs::DirBuilder::new().mode(0o700).create(data_dir)?;
            super::initialize::run(data_dir, postgres_archive, false).await?;
            let cluster = ManagedCluster::open(&data_dir.join("postgres"))?;
            let restored =
                super::provision::restore(&cluster, &mut archive, &manifest.database).await;
            let stopped = super::supervisor::stop(&cluster, Duration::from_secs(30)).await;
            restored.context("managed restore failed; target retained for inspection")?;
            stopped.context(
                "restored managed target could not be stopped; configuration remains unchanged",
            )?;
        }
    }
    ensure!(
        deployment_backup::verify(source)? == manifest,
        "backup changed during restore; target retained without publication"
    );
    if manifest.knowledge.is_some() {
        KnowledgeBackup::verify_restored(&source.join("knowledge"), &stage.join("knowledge"))?;
        if let Some(selection) = &selection {
            selection.verify(&source.join("policy"), &stage.join("policy"))?;
        } else {
            KnowledgeBackup::verify_restored(&source.join("policy"), &stage.join("policy"))?;
        }
    }
    let receipt = Receipt {
        version: 2,
        restored_at: chrono::Utc::now(),
        database_id: manifest.database.database_id,
        generation: manifest.database.generation,
        corpus_id: manifest.knowledge.as_ref().map(|k| k.corpus_id),
        source_dump_sha256: manifest.database.sha256,
        knowledge_dir: manifest
            .knowledge
            .as_ref()
            .map(|_| destination.join("knowledge")),
        policy_dir: manifest
            .knowledge
            .as_ref()
            .map(|_| destination.join("policy")),
        destination: destination.clone(),
        configuration_switched: false,
        selection_rebase: selection.map(|selection| selection.evidence),
        source_configuration_sha256: manifest.configuration.as_ref().map(|c| c.sha256.clone()),
    };
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("restore.json"))?;
    output.write_all(&serde_json::to_vec_pretty(&receipt)?)?;
    output.sync_all()?;
    File::open(&stage)?.sync_all()?;
    super::package::publish(&stage, &destination)?;
    File::open(&parent)?.sync_all()?;
    Ok(receipt)
}
