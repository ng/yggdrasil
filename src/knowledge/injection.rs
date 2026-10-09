//! Local injection eligibility and best-effort session receipts, independent of
//! PostgreSQL. Receipts suppress duplicate emissions, never authorize content.
use super::{
    document::digest,
    matching::Filters,
    runtime::Context,
    store::{RevisionedDocument, Snapshot},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    version: u32,
    corpus: Uuid,
    session: String,
    // Simple UUIDs keep 10,000 digest entries below the 1 MiB control limit.
    documents: BTreeMap<String, String>,
}
fn diagnostics(snapshot: &Snapshot) {
    for diagnostic in &snapshot.diagnostics {
        eprintln!("knowledge: {diagnostic}");
    }
}
fn current(
    context: &Context,
    selected: &[RevisionedDocument],
    filters: &Filters<'_>,
) -> Result<Vec<RevisionedDocument>> {
    let snapshot = context
        .service
        .revalidate_rules(selected, filters, chrono::Utc::now())?;
    diagnostics(&snapshot);
    Ok(snapshot
        .documents
        .into_iter()
        .filter(|doc| {
            doc.document
                .profile()
                .ok()
                .flatten()
                .is_some_and(|p| p.file_glob.is_some())
        })
        .collect())
}

pub fn for_edit(
    context: &Context,
    file: &str,
    agent: &str,
    session: &str,
) -> Result<Vec<RevisionedDocument>> {
    let repo = match context.repo(&std::env::current_dir()?) {
        Ok(repo) => Some(repo),
        Err(error) => {
            eprintln!("knowledge: repository rules omitted: {error}");
            None
        }
    };
    let filters = Filters {
        repo,
        file: Some(file),
        agent: Some(agent),
        ..Filters::default()
    };
    let mut selected = context.service.rules(&filters, chrono::Utc::now())?;
    diagnostics(&selected);
    selected.documents.retain(|doc| {
        (repo.is_some() || doc.key.repo.is_none())
            && doc
                .document
                .profile()
                .ok()
                .flatten()
                .is_some_and(|p| p.file_glob.is_some())
    });
    if selected.documents.is_empty() {
        return Ok(Vec::new());
    }
    if session.is_empty() {
        return current(context, &selected.documents, &filters);
    }
    let claimed = (|| -> Result<Vec<RevisionedDocument>> {
        ensure!(
            session.len() <= 4096 && context.user.len() <= 4096,
            "knowledge session identity exceeds limit"
        );
        let identity = digest(&serde_json::to_vec(&(
            context.mappings.corpus_id,
            &context.user,
            session,
        ))?);
        let store = context.session_store()?;
        store.update_session_control(&format!("{identity}.json"), |prior| {
            let mut seen = match prior.map(serde_json::from_str::<Seen>).transpose() {
                Ok(Some(seen))
                    if seen.version == 1
                        && seen.corpus == context.mappings.corpus_id
                        && seen.session == identity
                        && seen.documents.len() <= 10_000
                        && seen.documents.iter().all(|(id, d)| {
                            id.len() == 32
                                && Uuid::parse_str(id).is_ok()
                                && d.len() == 64
                                && d.bytes().all(|b| b.is_ascii_hexdigit())
                        }) =>
                {
                    seen
                }
                prior => {
                    if !matches!(prior, Ok(None)) {
                        eprintln!("knowledge: invalid session receipt; eligible rules may repeat");
                    }
                    Seen {
                        version: 1,
                        corpus: context.mappings.corpus_id,
                        session: identity.clone(),
                        documents: BTreeMap::new(),
                    }
                }
            };
            // Revalidate after acquiring the writer lease: contenders cannot
            // consume cached eligibility or each claim the same digest.
            let documents = current(context, &selected.documents, &filters)?;
            let mut fresh = Vec::new();
            for doc in documents {
                let id = doc.key.id.simple().to_string();
                let digest = doc.document.approval_digest()?;
                if seen.documents.get(&id) == Some(&digest) {
                    continue;
                }
                seen.documents.insert(id, digest);
                fresh.push(doc);
            }
            ensure!(
                seen.documents.len() <= 10_000,
                "knowledge session exceeds 10000 rules"
            );
            Ok((serde_json::to_string(&seen)?, fresh))
        })
    })();
    match claimed {
        Ok(documents) => Ok(documents),
        Err(_) => {
            // A full disk or inaccessible disposable state must not erase valid
            // knowledge. Re-read eligibility; never fall back to stale bytes.
            eprintln!("knowledge: session deduplication unavailable; eligible rules may repeat");
            current(context, &selected.documents, &filters)
        }
    }
}
