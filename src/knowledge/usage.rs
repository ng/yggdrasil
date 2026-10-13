//! Operational usage publication and explicit refresh. Offline deliveries are
//! not spooled, and telemetry never controls knowledge eligibility.
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
    let phase = super::timing::Phase::start("usage_sql");
    let recorded = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        let totals =
            telemetry::record_batch(&mut lease, context.mappings.corpus_id, applications).await?;
        lease.commit().await?;
        Ok::<_, anyhow::Error>(totals)
    })
    .await;
    drop(phase);
    let _phase = super::timing::Phase::start("usage_cache");
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

/// Totals absent from SQL or missing an imported baseline retain their previous
/// local value. Observation is not authorization and never changes a document.
#[derive(Debug, serde::Serialize)]
pub struct RefreshReport {
    pub requested: usize,
    pub observed: usize,
    pub missing: usize,
    pub missing_baseline: usize,
    pub unrepresentable: usize,
}

pub async fn refresh(context: &Context, pool: &sqlx::PgPool) -> anyhow::Result<RefreshReport> {
    use super::{legacy, matching::Filters};
    use anyhow::ensure;
    let active = context.service.list_rules(&Filters::default())?;
    let pending = context.service.pending(None)?;
    ensure!(
        active.diagnostics.is_empty() && pending.diagnostics.is_empty(),
        "repair knowledge diagnostics before refreshing usage"
    );
    let mut requested = std::collections::BTreeMap::new();
    for doc in active.documents.iter().chain(&pending.documents) {
        requested.insert(doc.key.id, legacy::is_imported(&doc.document)?);
    }
    ensure!(
        requested.len() <= 10_000,
        "usage refresh exceeds 10000 documents"
    );
    let mut lease = context.storage_lease(pool).await?;
    let ids: Vec<_> = requested.keys().copied().collect();
    let totals = sqlx::query_as::<_, telemetry::Totals>(
        "SELECT corpus_id, document_id, COALESCE(imported_count, 0)::bigint + observed_count AS applied_count, \
         GREATEST(imported_last_applied_at, observed_last_applied_at) AS last_applied_at, \
         imported_count IS NOT NULL AS baseline_imported FROM public.knowledge_usage WHERE corpus_id=$1 AND document_id=ANY($2)"
    ).bind(context.mappings.corpus_id).bind(&ids).fetch_all(&mut *lease).await?;
    let mut report = RefreshReport {
        requested: requested.len(),
        observed: 0,
        missing: requested.len() - totals.len(),
        missing_baseline: 0,
        unrepresentable: 0,
    };
    let mut eligible = Vec::new();
    for total in totals {
        if requested[&total.document_id] && !total.baseline_imported {
            report.missing_baseline += 1;
        } else if total.legacy_usage().is_err() {
            report.unrepresentable += 1;
        } else {
            eligible.push(total);
        }
    }
    // Complete the guarded SQL observation before taking the policy writer
    // lock; this follows the same lock ordering as post-emission publication.
    lease.commit().await?;
    report.observed = eligible.len();
    if !eligible.is_empty() {
        context.cache_usage(&eligible)?;
    }
    Ok(report)
}
