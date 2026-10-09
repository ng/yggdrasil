//! Resumable private staging, deliberately separate from authoritative publication.
use super::{
    document::digest,
    inventory,
    legacy::{Mappings, Usage},
    store::{ExpectedRevision, Key, KnowledgeStore},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;

const PLAN: &str = ".export-plan.json";
const COMPLETE: &str = ".export-complete.json";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub key: Key,
    pub source_digest: String,
    pub document_digest: String,
    pub usage: Option<Usage>,
}
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub database_id: Uuid,
    pub generation: i64,
    pub corpus_id: Uuid,
    /// Exact mapping data, including explicit empty-user mappings, for recovery.
    pub mappings: serde_json::Value,
    pub entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    version: u32,
    manifest_digest: String,
    documents: usize,
}

/// Requires an already fenced database and resolved mappings. No marker/config
/// changes, telemetry seeding or authoritative publication happen here.
pub async fn stage(pool: &PgPool, mappings: &Mappings, destination: &Path) -> Result<Manifest> {
    let mut entries = Vec::new();
    let report = inventory::visit(pool, Some(mappings), |source, row, doc, usage| {
        ensure!(
            source.backend == "fenced",
            "staged export requires fenced source"
        );
        entries.push(Entry {
            key: Key::from_document(doc)?,
            source_digest: row.source_digest.clone(),
            document_digest: row.document_digest.clone().unwrap(),
            usage: usage.cloned(),
        });
        Ok(())
    })
    .await?;
    ensure!(
        report.source.backend == "fenced",
        "staged export requires fenced source"
    );
    ensure!(
        report.rows_verified,
        "resolve all inventory issues before staging"
    );
    let manifest = Manifest {
        version: 1,
        database_id: report.source.database_id,
        generation: report.source.generation,
        corpus_id: mappings.corpus_id,
        mappings: serde_json::to_value(mappings)?,
        entries,
    };
    let plan = serde_json::to_string(&manifest)?;
    ensure!(
        plan.len() <= 64 * 1024 * 1024,
        "export manifest exceeds 64 MiB limit"
    );
    let store = KnowledgeStore::open(destination, true)?;
    let _export = store.operation_lock(".export.lock")?;
    // Persist every expected source/document digest before writing any document.
    // Repeated input produces identical bytes despite a new SQL snapshot ID.
    store.retain_artifact(PLAN, &plan, true)?;
    let expected: BTreeMap<_, _> = manifest.entries.iter().map(|e| (e.key.id, e)).collect();
    let mut observed = 0;
    let reread = inventory::visit(pool, Some(mappings), |source, row, document, usage| {
        ensure!(
            source.backend == "fenced"
                && source.database_id == manifest.database_id
                && source.generation == manifest.generation
                && source.corpus_id == Some(manifest.corpus_id),
            "source generation or phase changed before export"
        );
        let key = Key::from_document(document)?;
        let entry = expected
            .get(&key.id)
            .ok_or_else(|| anyhow::anyhow!("source inventory changed before export"))?;
        ensure!(
            entry.key == key
                && entry.source_digest == row.source_digest
                && Some(&entry.document_digest) == row.document_digest.as_ref()
                && entry.usage.as_ref() == usage,
            "source row changed before export"
        );
        match store.get(key)? {
            Some(current) => ensure!(
                current.revision == entry.document_digest,
                "staged document was independently edited; refusing overwrite"
            ),
            None => {
                store.put(document, ExpectedRevision::Absent)?;
            }
        }
        observed += 1;
        Ok(())
    })
    .await?;
    ensure!(
        reread.rows_verified
            && reread.source.backend == "fenced"
            && reread.source.database_id == manifest.database_id
            && reread.source.generation == manifest.generation
            && reread.source.corpus_id == Some(manifest.corpus_id)
            && observed == expected.len(),
        "source inventory or generation changed during export"
    );
    verify_documents(&store, &manifest)?;
    let receipt = serde_json::to_string(&Completion {
        version: 1,
        manifest_digest: digest(plan.as_bytes()),
        documents: observed,
    })?;
    store.retain_artifact(COMPLETE, &receipt, false)?;
    Ok(manifest)
}

fn verify_documents(store: &KnowledgeStore, manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.version == 1 && manifest.generation >= 1,
        "unsupported export manifest version/generation"
    );
    let mappings: Mappings = serde_json::from_value(manifest.mappings.clone())?;
    ensure!(
        mappings.database_id == manifest.database_id && mappings.corpus_id == manifest.corpus_id,
        "manifest identity disagrees with saved mappings"
    );
    let unique: std::collections::BTreeSet<_> = manifest.entries.iter().map(|e| e.key.id).collect();
    ensure!(
        unique.len() == manifest.entries.len(),
        "duplicate manifest document UUID"
    );
    for entry in &manifest.entries {
        ensure!(
            entry.source_digest.len() == 64
                && entry.source_digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source digest"
        );
        match (&entry.usage, entry.key.kind) {
            (Some(usage), super::store::Kind::Learning) => ensure!(
                usage.corpus_id == manifest.corpus_id && usage.document_id == entry.key.id,
                "manifest usage identity mismatch"
            ),
            (None, super::store::Kind::Note) => {}
            _ => anyhow::bail!("manifest usage does not match document kind"),
        }
    }
    let snapshot = store.snapshot();
    ensure!(
        snapshot.diagnostics.is_empty(),
        "staging diagnostics: {:?}",
        snapshot.diagnostics
    );
    ensure!(
        snapshot.documents.len() == manifest.entries.len(),
        "staging inventory differs from manifest"
    );
    let actual: BTreeMap<_, _> = snapshot.documents.iter().map(|d| (d.key.id, d)).collect();
    for entry in &manifest.entries {
        let doc = actual
            .get(&entry.key.id)
            .ok_or_else(|| anyhow::anyhow!("staged document missing"))?;
        ensure!(
            doc.key == entry.key && doc.revision == entry.document_digest,
            "staged document differs from manifest"
        );
    }
    Ok(())
}

/// A completion receipt alone is insufficient: always validate current staged
/// bytes. Future publication must additionally verify the live source generation.
pub fn verify(destination: &Path) -> Result<Manifest> {
    let store = KnowledgeStore::open(destination, false)?;
    let _export = store.operation_lock(".export.lock")?;
    let plan = store
        .read_artifact(PLAN)?
        .ok_or_else(|| anyhow::anyhow!("export manifest missing"))?;
    let receipt = store
        .read_artifact(COMPLETE)?
        .ok_or_else(|| anyhow::anyhow!("export incomplete"))?;
    let receipt: Completion = serde_json::from_str(&receipt)?;
    ensure!(
        receipt.version == 1 && receipt.manifest_digest == digest(plan.as_bytes()),
        "export receipt mismatch"
    );
    let manifest: Manifest = serde_json::from_str(&plan)?;
    ensure!(
        receipt.documents == manifest.entries.len(),
        "export count mismatch"
    );
    verify_documents(&store, &manifest)?;
    Ok(manifest)
}

/// Publish a complete, independently verifiable bundle while the SQL generation
/// remains fenced. Does not select OKF, grant trust, seed telemetry or alter SQL.
/// `archive` is retained recovery evidence, not a disposable temporary directory.
/// Retries verify exact existing bytes; no independently edited target is replaced.
pub async fn publish(
    pool: &PgPool,
    staging: &Path,
    archive: &Path,
    destination: &Path,
) -> Result<Manifest> {
    use super::store::{BackupEntry, KnowledgeBackup};
    fn target_path(path: &Path) -> Result<std::path::PathBuf> {
        ensure!(path.is_absolute(), "absolute publication paths required");
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("publication parent required"))?
            .canonicalize()?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("publication filename required"))?;
        Ok(parent.join(name))
    }
    let stage_path = staging.canonicalize()?;
    let archive_path = target_path(archive)?;
    let target = target_path(destination)?;
    ensure!(
        !target.starts_with(&stage_path)
            && !stage_path.starts_with(&target)
            && !target.starts_with(&archive_path)
            && !archive_path.starts_with(&target),
        "publication destination must be separate from staging and archive"
    );
    let manifest = verify(staging)?;
    let mappings: Mappings = serde_json::from_value(manifest.mappings.clone())?;
    let mut tx = pool.begin().await?;
    let expected: BTreeMap<_, _> = manifest.entries.iter().map(|e| (e.key.id, e)).collect();
    let mut observed = 0;
    let report = inventory::visit_transaction(&mut tx, Some(&mappings), |_, row, doc, usage| {
        let key = Key::from_document(doc)?;
        let entry = expected
            .get(&key.id)
            .ok_or_else(|| anyhow::anyhow!("source inventory changed before publication"))?;
        ensure!(
            entry.key == key
                && entry.source_digest == row.source_digest
                && Some(&entry.document_digest) == row.document_digest.as_ref()
                && entry.usage.as_ref() == usage,
            "source row differs from publication manifest"
        );
        observed += 1;
        Ok(())
    })
    .await?;
    ensure!(
        report.rows_verified
            && observed == expected.len()
            && report.source.backend == "fenced"
            && report.source.database_id == manifest.database_id
            && report.source.corpus_id == Some(manifest.corpus_id)
            && report.source.generation == manifest.generation,
        "publication requires unchanged, completely verified fenced source"
    );
    match std::fs::symlink_metadata(archive) {
        Ok(_) => {
            KnowledgeBackup::verify(archive)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            KnowledgeStore::open(staging, false)?.backup(archive)?;
        }
        Err(e) => return Err(e.into()),
    }
    // Validate the immutable archive without opening corpus locks or a mutable
    // snapshot reader inside it. Every file must be one of the verified export
    // documents or the two exact manifest/receipt files.
    let source = KnowledgeStore::open(staging, false)?;
    ensure!(
        verify(staging)? == manifest,
        "staging export changed during capture"
    );
    let mut files: BTreeMap<String, String> = manifest
        .entries
        .iter()
        .map(|e| {
            (
                e.key.relative_path().to_string_lossy().into_owned(),
                e.document_digest.clone(),
            )
        })
        .collect();
    for name in [PLAN, COMPLETE] {
        let bytes = source
            .read_artifact(name)?
            .ok_or_else(|| anyhow::anyhow!("export evidence missing"))?;
        files.insert(name.into(), digest(bytes.as_bytes()));
    }
    let archived = KnowledgeBackup::verify(archive)?;
    let actual: BTreeMap<_, _> = archived
        .entries
        .iter()
        .filter_map(|(name, entry)| match entry {
            BackupEntry::File { sha256, .. } => Some((name.clone(), sha256.clone())),
            BackupEntry::Directory => None,
        })
        .collect();
    ensure!(
        actual == files,
        "publication archive differs from verified export"
    );
    KnowledgeBackup::verify_restored(archive, staging)?;
    match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            KnowledgeBackup::verify_restored(archive, destination)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            KnowledgeBackup::restore(archive, destination)?;
        }
        Err(e) => return Err(e.into()),
    }
    ensure!(
        verify(destination)? == manifest,
        "published export differs from manifest"
    );
    // A lost database lease makes publication uncertain. Leave the unselected
    // directory and archive for inspection; never acknowledge or erase it.
    sqlx::query("SELECT 1").execute(&mut *tx).await?;
    tx.rollback().await?;
    Ok(manifest)
}
