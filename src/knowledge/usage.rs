//! Best-effort observation after output. No retries or offline spool: an outage
//! can undercount delivery, but telemetry never controls knowledge eligibility.
use super::{
    runtime::Context,
    telemetry::{self, Application},
};

pub async fn after_emission(
    context: &Context,
    mut lease: sqlx::Transaction<'static, sqlx::Postgres>,
    applications: &[Application],
) {
    if applications.is_empty() {
        return;
    }
    let recorded = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        let totals =
            telemetry::record_batch(&mut lease, context.mappings.corpus_id, applications).await?;
        lease.commit().await?;
        Ok::<_, anyhow::Error>(totals)
    })
    .await;
    match recorded {
        Ok(Ok(totals)) => {
            // The DB transaction has ended before taking the policy writer lock.
            // Failure here leaves durable counts available for later refresh.
            if !totals.is_empty() && context.cache_usage(&totals).is_err() {
                eprintln!("knowledge: usage recorded; local counter cache unavailable");
            }
        }
        _ => eprintln!("knowledge: optional usage telemetry unavailable"),
    }
}
