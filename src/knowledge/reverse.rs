//! Lossless reverse-import candidates from CURRENT corpus bytes and an owner-only
//! transactional SQL apply primitive. The migration workflow must recapture these
//! revisions and usage under its leases; this module never activates SQL storage.
use super::{
    document::digest,
    export::Manifest,
    legacy::{self, Mappings, Usage},
    store::{Key, Kind, Snapshot},
};
use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct CurrentDocument {
    pub key: Key,
    pub revision: String,
}
#[derive(Debug, Serialize)]
pub struct Candidate {
    pub version: u32,
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    pub export_generation: i64,
    pub export_digest: String,
    pub documents: Vec<CurrentDocument>,
    pub deleted: Vec<Key>,
    pub notes: Vec<Value>,
    pub learnings: Vec<Value>,
}

/// Requires a complete current snapshot and explicit current usage for every
/// rule, including new rules. Missing telemetry cannot become fabricated zeros.
/// Export baselines constrain totals; they never replace post-cutover usage.
pub fn build(
    original: &Manifest,
    current: &Snapshot,
    usage: &BTreeMap<Uuid, Usage>,
) -> Result<Candidate> {
    ensure!(
        original.version == 1 && original.generation > 0,
        "unsupported export manifest"
    );
    let mappings: Mappings = serde_json::from_value(original.mappings.clone())?;
    ensure!(
        mappings.database_id == original.database_id && mappings.corpus_id == original.corpus_id,
        "export mapping identity mismatch"
    );
    ensure!(
        current.diagnostics.is_empty(),
        "current corpus is incomplete or invalid: {:?}",
        current.diagnostics
    );
    let before: BTreeMap<_, _> = original.entries.iter().map(|e| (e.key.id, e)).collect();
    ensure!(
        before.len() == original.entries.len(),
        "duplicate original document UUID"
    );
    for entry in &original.entries {
        ensure!(
            [&entry.source_digest, &entry.document_digest]
                .iter()
                .all(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid export digest"
        );
        match (&entry.usage, entry.key.kind) {
            (Some(baseline), Kind::Learning) => ensure!(
                baseline.corpus_id == original.corpus_id && baseline.document_id == entry.key.id,
                "export usage identity mismatch"
            ),
            (None, Kind::Note) => {}
            _ => anyhow::bail!("export baseline does not match document kind"),
        }
    }
    let mut seen = BTreeSet::new();
    let mut candidate = Candidate {
        version: 1,
        database_id: original.database_id,
        corpus_id: original.corpus_id,
        export_generation: original.generation,
        export_digest: digest(&serde_json::to_vec(original)?),
        documents: Vec::new(),
        deleted: Vec::new(),
        notes: Vec::new(),
        learnings: Vec::new(),
    };
    for current in &current.documents {
        let key = Key::from_document(&current.document)?;
        ensure!(
            key == current.key && seen.insert(key.id),
            "current document key mismatch or duplicate UUID"
        );
        ensure!(
            current.revision.len() == 64 && current.revision.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid current document revision"
        );
        if let Some(old) = before.get(&key.id) {
            ensure!(
                old.key.kind == key.kind,
                "document kind changed after cutover"
            );
        }
        let (mut row, user) = match key.kind {
            Kind::Note => {
                let (row, user) = legacy::reverse_note(&current.document, &mappings)?;
                (serde_json::to_value(row)?, user)
            }
            Kind::Learning => {
                let totals = usage
                    .get(&key.id)
                    .ok_or_else(|| anyhow!("current usage missing for rule {}", key.id))?;
                if let Some(old) = before.get(&key.id) {
                    let baseline = old
                        .usage
                        .as_ref()
                        .ok_or_else(|| anyhow!("export rule baseline missing"))?;
                    ensure!(
                        baseline.corpus_id == original.corpus_id
                            && baseline.document_id == key.id
                            && totals.applied_count >= baseline.applied_count
                            && totals.last_applied_at >= baseline.last_applied_at,
                        "current usage is below saved migration baseline"
                    );
                }
                let (row, user) = legacy::reverse_learning(&current.document, totals, &mappings)?;
                (serde_json::to_value(row)?, user)
            }
        };
        row.as_object_mut()
            .unwrap()
            .insert("user_id".into(), Value::String(user));
        match key.kind {
            Kind::Note => candidate.notes.push(row),
            Kind::Learning => candidate.learnings.push(row),
        }
        candidate.documents.push(CurrentDocument {
            key,
            revision: current.revision.clone(),
        });
    }
    candidate.deleted = before
        .values()
        .filter(|e| !seen.contains(&e.key.id))
        .map(|e| e.key)
        .collect();
    candidate.documents.sort_by_key(|d| d.key.id);
    candidate
        .notes
        .sort_by_key(|r| r["memory_id"].as_str().unwrap().to_owned());
    candidate
        .learnings
        .sort_by_key(|r| r["learning_id"].as_str().unwrap().to_owned());
    Ok(candidate)
}

/// Capture authoritative committed usage and build a current reverse candidate.
/// The caller must already quiesce filesystem writers and retain the current
/// bundle backup/leases. Keep this READ COMMITTED transaction through apply and
/// commit: its migration and telemetry locks prevent totals from changing.
/// Offline deliveries were never recorded and cannot be reconstructed here.
pub async fn capture_on(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    original: &Manifest,
    current: &Snapshot,
    fenced_generation: i64,
) -> Result<Candidate> {
    ensure!(
        original.version == 1 && fenced_generation > original.generation,
        "reverse capture requires a later fenced generation"
    );
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut **transaction)
        .await?;
    ensure!(
        isolation == "read committed",
        "reverse capture requires READ COMMITTED isolation"
    );
    sqlx::query("SELECT pg_advisory_xact_lock(1497843531, 1)")
        .execute(&mut **transaction)
        .await?;
    let marker: (Uuid, Option<Uuid>, i64, i32, String) = sqlx::query_as(
        "SELECT database_id,corpus_id,generation,minimum_client,backend FROM public.knowledge_storage WHERE singleton FOR UPDATE"
    ).fetch_one(&mut **transaction).await?;
    ensure!(
        marker.0 == original.database_id
            && marker.1 == Some(original.corpus_id)
            && marker.2 == fenced_generation
            && marker.3 > 0
            && marker.3 <= super::guard::CLIENT_PROTOCOL
            && marker.4 == "fenced",
        "reverse capture requires the expected compatible fenced generation"
    );
    // Also drain direct telemetry callers which do not take the generation
    // lease. Table-level SHARE blocks inserts/updates/deletes, including rows
    // that do not yet exist; row locks alone cannot stabilize absent totals.
    sqlx::query("LOCK TABLE public.knowledge_usage IN SHARE MODE")
        .execute(&mut **transaction)
        .await?;
    let ids: Vec<_> = current
        .documents
        .iter()
        .filter(|d| d.key.kind == Kind::Learning)
        .map(|d| d.key.id)
        .collect();
    #[derive(sqlx::FromRow)]
    struct Recorded {
        document_id: Uuid,
        imported_count: Option<i32>,
        imported_last_applied_at: Option<chrono::DateTime<chrono::Utc>>,
        observed_count: i64,
        last_applied_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows: Vec<Recorded> = sqlx::query_as(
        "SELECT document_id,imported_count,imported_last_applied_at,observed_count, \
         GREATEST(imported_last_applied_at,observed_last_applied_at) AS last_applied_at \
         FROM public.knowledge_usage WHERE corpus_id=$1 AND document_id=ANY($2)",
    )
    .bind(original.corpus_id)
    .bind(&ids)
    .fetch_all(&mut **transaction)
    .await?;
    let rows: BTreeMap<_, _> = rows.into_iter().map(|r| (r.document_id, r)).collect();
    let originals: BTreeMap<_, _> = original.entries.iter().map(|e| (e.key.id, e)).collect();
    let mut usage = BTreeMap::new();
    for id in ids {
        let row = rows.get(&id);
        match originals.get(&id) {
            Some(entry) => {
                let baseline = entry
                    .usage
                    .as_ref()
                    .ok_or_else(|| anyhow!("missing export rule baseline"))?;
                let row = row.ok_or_else(|| anyhow!("missing imported telemetry for rule {id}"))?;
                ensure!(
                    row.imported_count == Some(baseline.applied_count)
                        && row.imported_last_applied_at == baseline.last_applied_at,
                    "imported telemetry differs from export baseline for rule {id}"
                );
            }
            None => ensure!(
                row.is_none_or(
                    |r| r.imported_count.is_none() && r.imported_last_applied_at.is_none()
                ),
                "unexpected imported telemetry for new rule {id}"
            ),
        }
        let count = match row {
            Some(row) => {
                ensure!(row.observed_count >= 0, "negative observed telemetry");
                i64::from(row.imported_count.unwrap_or(0))
                    .checked_add(row.observed_count)
                    .ok_or_else(|| anyhow!("telemetry count overflow"))?
                    .try_into()?
            }
            None => 0, // Proven absent under the table lock, and not an imported rule.
        };
        usage.insert(
            id,
            Usage {
                corpus_id: original.corpus_id,
                document_id: id,
                applied_count: count,
                last_applied_at: row.and_then(|r| r.last_applied_at),
            },
        );
    }
    build(original, current, &usage)
}

/// Capture private-corpus rollback rows from a retained exact corpus/policy
/// backup, with database totals frozen in the same transaction. The caller must
/// hold the selection lease and keep `recovery` alive through apply and commit.
/// This does not resolve shared Git trees or activate SQL storage.
pub async fn capture_recovery_on(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    original: &Manifest,
    recovery: &super::store::PairedBackup,
    fenced_generation: i64,
) -> Result<Candidate> {
    let current = recovery.snapshot()?;
    let candidate = capture_on(transaction, original, &current, fenced_generation).await?;
    recovery.verify_sources()?;
    Ok(candidate)
}

#[derive(Serialize)]
pub struct SharedCandidate {
    pub commit: String,
    pub candidate: Candidate,
}

/// Capture shared rollback from a retained transport/policy backup and a freshly
/// verified authoritative commit. Keep the recovery and selection leases through
/// apply/commit, quiesce every remote writer, and recheck the remote before commit.
pub async fn capture_shared_recovery_on(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    original: &Manifest,
    transport: &super::shared::SharedGit,
    recovery: &super::store::PairedBackup,
    fenced_generation: i64,
) -> Result<SharedCandidate> {
    let snapshot = transport.recovery_snapshot(recovery)?;
    let candidate = capture_on(transaction, original, &snapshot.current, fenced_generation).await?;
    transport.verify_recovery(recovery, &snapshot.commit)?;
    Ok(SharedCandidate {
        commit: snapshot.commit,
        candidate,
    })
}

/// Low-level fenced SQL apply. The owning migration workflow must retain the
/// current bundle/usage evidence and its filesystem leases, then commit this
/// transaction only after all rollback checks pass. This never selects SQL.
pub async fn apply_on(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candidate: &Candidate,
    fenced_generation: i64,
) -> Result<()> {
    ensure!(
        candidate.version == 1 && fenced_generation > candidate.export_generation,
        "reverse apply requires a later fenced generation"
    );
    sqlx::query("SELECT public.ygg_knowledge_reverse_import($1,$2,$3,$4,$5)")
        .bind(candidate.database_id)
        .bind(candidate.corpus_id)
        .bind(fenced_generation)
        .bind(serde_json::to_value(&candidate.notes)?)
        .bind(serde_json::to_value(&candidate.learnings)?)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

/// Exact recovery evidence saved by the migration journal before apply.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryEvidence {
    pub version: u32,
    pub candidate_sha256: String,
    pub corpus_revision: String,
    pub policy_revision: String,
    pub shared_commit: Option<String>,
}
impl RecoveryEvidence {
    pub fn new(
        candidate: &Candidate,
        recovery: &super::store::PairedBackup,
        shared_commit: Option<String>,
    ) -> Result<Self> {
        recovery.verify_sources()?;
        Ok(Self {
            version: 1,
            candidate_sha256: digest(&serde_json::to_vec(candidate)?),
            corpus_revision: recovery.corpus().revision.clone(),
            policy_revision: recovery.policy().revision.clone(),
            shared_commit,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied,
    PreviouslyApplied,
}

/// Apply once and persist a receipt in the SAME caller transaction. A repeated
/// operation ID verifies its original request and restored row hashes without
/// repeating writes. Keep the durable operation ID/evidence across uncertain
/// commits; retain and revalidate filesystem/remote leases through final commit.
/// A returned outcome is provisional until that transaction commits.
pub async fn apply_once_on(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    operation: Uuid,
    candidate: &Candidate,
    evidence: &RecoveryEvidence,
    fenced_generation: i64,
) -> Result<ApplyOutcome> {
    ensure!(
        candidate.version == 1
            && evidence.version == 1
            && fenced_generation > candidate.export_generation,
        "unsupported reverse-import receipt request"
    );
    ensure!(
        evidence.candidate_sha256 == digest(&serde_json::to_vec(candidate)?),
        "candidate differs from retained recovery evidence"
    );
    let applied: bool =
        sqlx::query_scalar("SELECT public.ygg_knowledge_reverse_apply_once($1,$2,$3,$4,$5,$6,$7)")
            .bind(operation)
            .bind(candidate.database_id)
            .bind(candidate.corpus_id)
            .bind(fenced_generation)
            .bind(serde_json::to_value(evidence)?)
            .bind(serde_json::to_value(&candidate.notes)?)
            .bind(serde_json::to_value(&candidate.learnings)?)
            .fetch_one(&mut **transaction)
            .await?;
    Ok(if applied {
        ApplyOutcome::Applied
    } else {
        ApplyOutcome::PreviouslyApplied
    })
}
