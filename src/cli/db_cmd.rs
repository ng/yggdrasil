//! Database lifecycle commands resolve deployment without requiring AppConfig or
//! opening an external pool. No lifecycle command installs or initializes data.
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
