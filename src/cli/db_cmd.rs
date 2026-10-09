//! Database lifecycle commands and explicit read-only connection diagnostics.
//! Lifecycle commands never open an external pool or initialize data.
use crate::{
    config::database::{DatabaseTarget, DeploymentConfig},
    db::{runtime::ManagedCluster, supervisor},
};
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

fn target() -> Result<DatabaseTarget> {
    Ok(DeploymentConfig::load(std::env::vars().collect())?.database)
}

fn managed() -> Result<ManagedCluster> {
    match target()? {
        DatabaseTarget::ManagedLocal { data_dir } => {
            ManagedCluster::open(&data_dir.join("postgres"))
                .context("managed cluster unavailable; run ygg init to initialize it")
        }
        DatabaseTarget::External { .. } => anyhow::bail!(
            "external database selected; Yggdrasil does not manage its process lifecycle"
        ),
    }
}

pub async fn status(json: bool) -> Result<()> {
    let value = match target()? {
        DatabaseTarget::External { .. } => {
            serde_json::json!({"mode": "external", "state": "configured", "lifecycle": "unmanaged"})
        }
        DatabaseTarget::ManagedLocal { data_dir } => {
            let root = data_dir.join("postgres");
            match std::fs::symlink_metadata(&root) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    serde_json::json!({"mode": "managed", "state": "not_initialized", "root": root})
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {
                    let cluster = ManagedCluster::open(&root)?;
                    let supervisor = supervisor::inspect(&cluster).await.ok();
                    let postgres = cluster.status().await?;
                    serde_json::json!({"mode": "managed", "root": root, "cluster_id": cluster.id(),
                        "postgres": postgres, "supervisor_pid": supervisor.map(|s| s.supervisor_pid)})
                }
            }
        }
    };
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

pub async fn start(seconds: u64, json: bool) -> Result<()> {
    let cluster = managed()?;
    let reply = supervisor::start(
        &cluster,
        &std::env::current_exe()?,
        Duration::from_secs(seconds),
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string(&reply)?);
    } else {
        println!(
            "Managed database ready; supervisor {} owns cluster {}.",
            reply.supervisor_pid, reply.cluster_id
        );
    }
    Ok(())
}

pub async fn stop(seconds: u64, json: bool) -> Result<()> {
    supervisor::stop(&managed()?, Duration::from_secs(seconds)).await?;
    if json {
        println!(
            "{}",
            serde_json::json!({"mode": "managed", "state": "stopped"})
        );
    } else {
        println!("Managed database stopped.");
    }
    Ok(())
}

pub async fn serve(root: Option<PathBuf>, expected_id: Option<Uuid>) -> Result<()> {
    let cluster = match (root, expected_id) {
        (Some(root), Some(id)) => {
            let cluster = ManagedCluster::open(&root)?;
            ensure!(
                cluster.id() == id,
                "selected managed cluster identity changed"
            );
            cluster
        }
        (None, None) => managed()?,
        _ => anyhow::bail!("internal supervisor launch requires root and cluster ID together"),
    };
    supervisor::serve(cluster).await
}

/// Diagnose the runtime endpoint without invoking lifecycle or owner operations.
pub async fn diagnose(json: bool) -> Result<()> {
    let options = match target()? {
        DatabaseTarget::External { url } => crate::db::external::options(&url)?,
        DatabaseTarget::ManagedLocal { data_dir } => {
            let cluster = ManagedCluster::open(&data_dir.join("postgres"))
                .context("managed cluster unavailable; diagnostic does not initialize it")?;
            crate::db::provision::runtime_options(&cluster)
        }
    };
    let report = crate::db::diagnostics::inspect(&options).await?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    ensure!(
        !report.observed_incompatibility(),
        "backend changed between transactions; endpoint cannot preserve singleton authority"
    );
    Ok(())
}

pub async fn backup(
    destination: &std::path::Path,
    pg_bin: Option<&std::path::Path>,
    policy_dir: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let config = DeploymentConfig::load(std::env::vars().collect())?;
    let manifest =
        crate::db::deployment_backup::create(&config, destination, pg_bin, policy_dir).await?;
    if json {
        println!("{}", serde_json::to_string(&manifest)?);
    } else {
        println!("Backup verified and saved to {}", destination.display());
    }
    Ok(())
}

pub fn verify_backup(path: &std::path::Path, json: bool) -> Result<()> {
    let manifest = crate::db::deployment_backup::verify(path)?;
    if json {
        println!("{}", serde_json::to_string(&manifest)?);
    } else {
        println!("Backup integrity verified: {}", path.display());
    }
    Ok(())
}

/// Explicit recovery uses the selected target only; it never rewrites user config.
pub async fn restore(
    path: &std::path::Path,
    destination: &std::path::Path,
    pg_bin: Option<&std::path::Path>,
    postgres_archive: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let config = DeploymentConfig::load(std::env::vars().collect())?;
    let receipt =
        crate::db::deployment_restore::run(&config, path, destination, pg_bin, postgres_archive)
            .await
            .context("restore incomplete on failure; inspect retained target before retrying")?;
    if json {
        println!("{}", serde_json::to_string(&receipt)?);
    } else {
        println!(
            "Restore validated. Files: {}. Configuration unchanged; keep writers stopped until the deployment switch.",
            destination.display()
        );
    }
    Ok(())
}

pub async fn switch(
    backup: &std::path::Path,
    restored: &std::path::Path,
    target_config: &std::path::Path,
    resume: Option<Uuid>,
    json: bool,
) -> Result<()> {
    let outcome = crate::db::deployment_switch::run(
        backup,
        restored,
        target_config,
        std::env::vars().collect(),
        resume,
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string(&outcome)?);
    } else {
        println!(
            "Deployment configuration selected; switch {}. Restart participating clients with this configuration.",
            outcome.operation
        );
    }
    Ok(())
}
