use anyhow::{Result, ensure};
use std::{io::Read, path::Path};

pub async fn dry_run(mapping_file: Option<&Path>, json: bool) -> Result<()> {
    let mappings = mapping_file
        .map(|path| -> Result<crate::knowledge::legacy::Mappings> {
            let file = std::fs::File::open(path)?;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "mapping file exceeds 1 MiB limit"
            );
            Ok(serde_json::from_slice(&bytes)?)
        })
        .transpose()?;
    let config = crate::config::AppConfig::from_env()?;
    let pool = crate::db::connect(&config.database).await?;
    let report = crate::knowledge::inventory::assess(&pool, mappings.as_ref()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Source database: {} (generation {}, {})",
            report.source.database_id, report.source.generation, report.source.backend
        );
        println!(
            "{} notes, {} learnings, {} source owners, {} repositories",
            report.notes,
            report.learnings,
            report.owners.len(),
            report.repositories.len()
        );
        println!(
            "Source owners: {:?}",
            report.owners.keys().collect::<Vec<_>>()
        );
        println!("Legacy repositories: {:?}", report.repositories);
        for row in &report.rows {
            for issue in &row.issues {
                println!("{} {:?}: {issue}", row.table, row.id);
            }
        }
        println!(
            "Row verification: {}. No files published or storage mode changed.",
            if report.rows_verified {
                "passed"
            } else {
                "unresolved"
            }
        );
    }
    ensure!(
        report.rows_verified,
        "migration inventory has unresolved mappings or round-trip failures; review report"
    );
    Ok(())
}

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

pub fn rollback_status(journal: &Path, json: bool) -> Result<()> {
    let status = crate::knowledge::rollback::inspect(journal)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "Rollback {}: {} notes, {} learnings; fenced generation {}",
            status.operation, status.notes, status.learnings, status.fenced_generation
        );
        println!(
            "Local apply record: {}. Database outcome must be verified when resuming.",
            status.local_apply_recorded
        );
    }
    Ok(())
}

pub fn fence_local(generation: i64, json: bool) -> Result<()> {
    let (config, _) = crate::config::database::KnowledgeConfig::load(std::env::vars().collect())?;
    let report = crate::knowledge::fence::local(&config, generation)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Local OKF commands fenced at source generation {} (operation {}).",
            report.source_generation, report.operation
        );
        println!("Database state and other hosts still require migration verification.");
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

pub async fn migrate(
    plan: &Path,
    journal: &Path,
    pg_bin: Option<&Path>,
    abort: bool,
    json: bool,
) -> Result<()> {
    let mut bytes = Vec::new();
    std::fs::File::open(plan)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "migration plan exceeds 1 MiB limit"
    );
    let plan = serde_json::from_slice(&bytes)?;
    let config =
        crate::config::database::DeploymentConfig::load_maintenance(std::env::vars().collect())?;
    let journal = crate::knowledge::migration::Journal::prepare(journal, plan, &config)?;
    let report = if abort {
        journal.abort(&config).await?
    } else {
        journal.execute(&config, pg_bin).await?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Knowledge {} at generation {}. Retained operation: {}",
            report.state,
            report.generation,
            report.journal.display()
        );
    }
    Ok(())
}

pub async fn rollback(
    plan: &Path,
    journal: &Path,
    pg_bin: Option<&Path>,
    json: bool,
) -> Result<()> {
    let mut bytes = Vec::new();
    std::fs::File::open(plan)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "rollback plan exceeds 1 MiB limit"
    );
    let plan = serde_json::from_slice(&bytes)?;
    let config =
        crate::config::database::DeploymentConfig::load_maintenance(std::env::vars().collect())?;
    let journal = crate::knowledge::reverse_migration::Journal::prepare(journal, plan, &config)?;
    let report = journal.execute(&config, pg_bin).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Knowledge restored to SQL at generation {}. Retained recovery: {}",
            report.generation,
            report.journal.display()
        );
    }
    Ok(())
}
