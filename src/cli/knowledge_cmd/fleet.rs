//! Explicit shared migration commands. Parse and pin every request before any
//! journal mutation or database connection; the library owns phase transitions.
use crate::{
    config::database::DeploymentConfig,
    knowledge::{
        document::digest,
        fleet::{
            journal::Journal,
            plan::ValidatedPlan,
            rollback::{ReconciliationPlan, RollbackPlan},
        },
    },
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(clap::Subcommand)]
pub enum Action {
    /// Inspect local receipts offline; these do not prove current SQL or host state
    Status,
    /// Reserve and prepare every host without fencing SQL or activating OKF
    Prepare,
    /// Execute or resume complete shared cutover, including every host's selection
    Execute,
    /// Cancel before the SQL fence and reconcile all declared hosts
    Cancel,
    /// Abort after the SQL fence but before OKF activation
    Abort,
    /// Restore current shared knowledge to SQL and deselect every host
    Rollback {
        #[arg(long)]
        request: PathBuf,
        /// SHA-256 of the exact reviewed rollback request bytes
        #[arg(long)]
        sha256: String,
    },
    /// Restore all rollback hosts before the global reverse fence, then release admission
    CancelRollback {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// Select an explicit descendant snapshot after reverse fencing; continue with rollback
    ReconcileRollback {
        #[arg(long)]
        rollback_request: PathBuf,
        #[arg(long)]
        rollback_sha256: String,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        sha256: String,
    },
}
fn read_pinned(path: &Path, expected: &str, limit: usize) -> Result<String> {
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a lowercase SHA-256 request digest"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "fleet request exceeds {limit} bytes");
    ensure!(
        digest(&bytes) == expected,
        "fleet request differs from supplied SHA-256"
    );
    Ok(String::from_utf8(bytes)?)
}
fn emit(value: Value, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Fleet {}. Operation: {}. Journal: {}",
            value["phase"].as_str().unwrap(),
            value["operation"].as_str().unwrap(),
            value["journal"].as_str().unwrap()
        );
    }
    Ok(())
}
pub async fn run(
    plan_path: &Path,
    directory: &Path,
    expected: &str,
    identity: Option<&Path>,
    action: Action,
    json_output: bool,
) -> Result<()> {
    ensure!(
        directory.to_str().is_some(),
        "fleet journal path must be UTF-8"
    );
    let plan = ValidatedPlan::parse(&read_pinned(plan_path, expected, 8 * 1024 * 1024)?)?;
    let reverse = match &action {
        Action::Rollback { request, sha256 } | Action::CancelRollback { request, sha256 } => Some(
            RollbackPlan::parse(&plan, &read_pinned(request, sha256, 1024 * 1024)?)?,
        ),
        Action::ReconcileRollback {
            rollback_request,
            rollback_sha256,
            ..
        } => Some(RollbackPlan::parse(
            &plan,
            &read_pinned(rollback_request, rollback_sha256, 1024 * 1024)?,
        )?),
        _ => None,
    };
    let reconciliation = match &action {
        Action::ReconcileRollback {
            request, sha256, ..
        } => Some(ReconciliationPlan::parse(
            reverse.as_ref().unwrap(),
            &read_pinned(request, sha256, 1024 * 1024)?,
        )?),
        _ => None,
    };
    let operation = plan.plan().operation;
    if matches!(action, Action::Status) {
        let journal = Journal::resume(directory, expected)?;
        return emit(
            json!({"version":1,"phase":"local_evidence_only","operation":operation,"request_sha256":expected,
            "journal":directory,"receipts":journal.local_receipts()?}),
            json_output,
        );
    }
    let config = DeploymentConfig::load_maintenance(std::env::vars().collect())?;
    let journal = Journal::prepare_for_deployment(directory, plan, &config)?;
    let pool = crate::db::maintenance_pool(&config).await?;
    let result:Result<(&str,Value)>=async {
        Ok(match action {
            Action::Prepare=>("prepared",json!({"sha256":journal.prepare_hosts(&config,&pool,identity).await?})),
            Action::Execute=>("finalized",serde_json::to_value(journal.finalize_hosts(&config,&pool,identity).await?)?),
            Action::Cancel=>("cancelled",json!({"sha256":journal.cancel_hosts(&pool,identity).await?})),
            Action::Abort=>("aborted",json!({"sha256":journal.abort_hosts(&config,&pool,identity).await?})),
            Action::Rollback{..}=>("sql_returned",serde_json::to_value(journal.deselect_rollback_hosts(reverse.as_ref().unwrap(),&config,&pool,identity).await?)?),
            Action::CancelRollback{..}=>("rollback_cancelled",json!({"sha256":journal.complete_rollback_cancellation(reverse.as_ref().unwrap(),&pool,identity).await?})),
            Action::ReconcileRollback{..}=>("rollback_reconciled",json!({"sha256":journal.reconcile_rollback_remote(reverse.as_ref().unwrap(),reconciliation.as_ref().unwrap(),&pool,identity).await?,"next_action":"rollback"})),
            Action::Status=>unreachable!(),
        })
    }.await;
    pool.close().await;
    let (phase, receipt) = result?;
    emit(
        json!({"version":1,"phase":phase,"operation":operation,"request_sha256":expected,"journal":directory,"receipt":receipt}),
        json_output,
    )
}
