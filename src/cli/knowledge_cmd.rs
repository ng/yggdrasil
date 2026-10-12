use anyhow::{Result, ensure};

pub fn sync(confirm_pending: bool, json: bool) -> Result<()> {
    let access =
        crate::knowledge::runtime::SharedAccess::from_environment(std::env::vars().collect())?;
    if confirm_pending {
        access.transport.confirm_pending()?;
    }
    let commit = access.transport.refresh()?.commit;
    if json {
        println!(
            "{}",
            serde_json::json!({"commit": commit, "confirmed": true})
        );
    } else {
        println!("Shared knowledge confirmed at {commit}");
    }
    Ok(())
}

pub fn pending(json: bool) -> Result<()> {
    let access =
        crate::knowledge::runtime::SharedAccess::from_environment(std::env::vars().collect())?;
    let pending = access.transport.pending_info()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
    } else if let Some(pending) = pending {
        println!("Pending commit {} (base {})", pending.commit, pending.base);
        for change in pending.changes {
            println!(
                "  {}: {} → {}",
                change.path,
                change.before.as_deref().unwrap_or("absent"),
                change.after.as_deref().unwrap_or("deleted")
            );
        }
    } else {
        println!("No pending shared publication.");
    }
    Ok(())
}
pub fn recover(commit: &str, retry: bool, json: bool) -> Result<()> {
    use crate::knowledge::shared::{RecoveryAction, RecoveryOutcome};
    let access =
        crate::knowledge::runtime::SharedAccess::from_environment(std::env::vars().collect())?;
    let result = access.transport.recover(
        commit,
        if retry {
            RecoveryAction::Retry
        } else {
            RecoveryAction::Discard
        },
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let action = match result.outcome {
            RecoveryOutcome::Confirmed => "Confirmed remote publication",
            RecoveryOutcome::Published => "Published recovered change",
            RecoveryOutcome::ArchivedUnconfirmed => {
                "Archived unconfirmed local draft; remote unchanged"
            }
        };
        println!("{action}: {}", result.commit);
        if let Some(reference) = result.archive_ref {
            println!("Retained at {reference}");
        }
    }
    Ok(())
}

pub async fn clients(json: bool) -> Result<()> {
    let config = crate::config::AppConfig::from_env()?;
    let report = crate::db::diagnostics::clients(&config).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{} live connections, {} compatibility blockers; generation {}.",
            report.clients.len(),
            report.live_blockers,
            report.generation
        );
        for client in &report.clients {
            println!(
                "Backend {} ({}): {:?}, protocol {:?}",
                client.backend_pid, client.role, client.compatibility, client.protocol
            );
        }
        println!(
            "Still requires: {}.",
            report.remaining_verification.join("; ")
        );
    }
    ensure!(
        report.live_blockers == 0,
        "unregistered or outdated live clients require upgrade/restart before cutover"
    );
    Ok(())
}

pub async fn refresh_usage(json: bool) -> Result<()> {
    let context = crate::knowledge::runtime::Context::from_environment(std::env::vars().collect())?
        .ok_or_else(|| anyhow::anyhow!("usage refresh requires selected OKF storage"))?;
    let config = crate::config::AppConfig::from_env()?;
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let pool = crate::db::connect(&config.database).await?;
        let result = crate::knowledge::usage::refresh(&context, &pool).await;
        pool.close().await;
        result
    })
    .await
    .map_err(|_| anyhow::anyhow!("usage refresh timed out"))??;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Observed usage for {} of {} rules; {} absent, {} missing imported baselines, {} outside legacy counter range. Local totals never regress.",
            report.observed,
            report.requested,
            report.missing,
            report.missing_baseline,
            report.unrepresentable
        );
    }
    Ok(())
}

/// Explicit inspection uses the selected private/shared corpus without SQL.
pub fn browse(path: Option<&str>, json: bool) -> Result<()> {
    let context = crate::knowledge::runtime::Context::from_environment(std::env::vars().collect())?
        .ok_or_else(|| anyhow::anyhow!("no selected OKF corpus"))?;
    let report = context.service.browse_documents(path)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if let Some(commit) = &report.shared_commit {
            eprintln!(
                "Shared revision {commit} (current: {})",
                report.shared_current.unwrap_or(false)
            );
        }
        for doc in &report.documents {
            if let Some(text) = &doc.text {
                print!("{text}");
            } else {
                println!("{}\t{}\t{}", doc.path, doc.document_type, doc.revision);
            }
        }
        for diagnostic in &report.diagnostics {
            eprintln!("knowledge browse: {diagnostic}");
        }
    }
    ensure!(
        report.diagnostics.is_empty(),
        "knowledge browse is incomplete; review diagnostics"
    );
    Ok(())
}

/// Create (or verify) the OKF corpus and bind the selected database to it.
pub async fn init(json: bool) -> Result<()> {
    let env: crate::config::database::Environment = std::env::vars().collect();
    let config = crate::config::database::DeploymentConfig::load_maintenance(env.clone())?;
    let pool = crate::db::maintenance_pool(&config).await?;
    let result = crate::knowledge::bootstrap::initialize(&config, &env, &pool).await;
    pool.close().await;
    let report = result?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.created {
        println!(
            "Knowledge initialized: corpus {} at {} (generation {}).",
            report.corpus_id,
            report.knowledge_dir.display(),
            report.generation
        );
    } else {
        println!(
            "Knowledge already initialized: corpus {} at {} (generation {}).",
            report.corpus_id,
            report.knowledge_dir.display(),
            report.generation
        );
    }
    Ok(())
}
