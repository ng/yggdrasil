//! Lossless reverse-import candidates from CURRENT corpus bytes. This module
//! neither selects SQL nor writes it: the fenced apply workflow must recapture
//! these revisions and usage under its leases before applying a candidate.
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
