//! Read-only migration assessment. A successful report does not authorize
//! cutover: a fenced export must re-read and verify its own source manifest.
use super::{
    document::digest,
    guard::CLIENT_PROTOCOL,
    legacy::{self, Mappings},
    store::Key,
};
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Serialize, FromRow)]
pub struct Source {
    pub database_id: Uuid,
    pub generation: i64,
    pub backend: String,
    pub corpus_id: Option<Uuid>,
    pub snapshot: String,
}
#[derive(Debug, Serialize)]
pub struct Row {
    pub table: String,
    pub id: Option<Uuid>,
    pub user_id: Option<String>,
    pub repo_id: Option<Uuid>,
    pub source_digest: String,
    pub target_path: Option<String>,
    pub document_digest: Option<String>,
    pub issues: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub dry_run: bool,
    pub source: Source,
    pub mapping_digest: Option<String>,
    pub notes: usize,
    pub learnings: usize,
    pub owners: BTreeMap<String, usize>,
    pub repositories: BTreeSet<Uuid>,
    pub rows: Vec<Row>,
    /// Every row was mapped and round-tripped. This is NOT cutover readiness.
    pub rows_verified: bool,
}

fn inspect(
    table: &str,
    raw: Value,
    mappings: Option<&Mappings>,
) -> (
    Row,
    Option<(super::document::Document, Option<legacy::Usage>)>,
) {
    let id_field = if table == "memories" {
        "memory_id"
    } else {
        "learning_id"
    };
    let mut result = Row {
        table: table.into(),
        id: raw[id_field].as_str().and_then(|s| s.parse().ok()),
        user_id: raw["user_id"].as_str().map(str::to_owned),
        repo_id: raw["repo_id"].as_str().and_then(|s| s.parse().ok()),
        source_digest: digest(&serde_json::to_vec(&raw).expect("JSON value serializes")),
        target_path: None,
        document_digest: None,
        issues: Vec::new(),
    };
    let mut extracted = None;
    let converted = (|| -> Result<()> {
        let mappings = mappings.ok_or_else(|| anyhow::anyhow!("explicit mapping file required"))?;
        let user = result
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("SQL owner field is missing or null"))?;
        let mut model = raw.clone();
        model
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("source row is not an object"))?
            .remove("user_id");
        let (document, expected, actual, usage) = if table == "memories" {
            let row: crate::models::memory::Memory = serde_json::from_value(model.clone())?;
            let document = legacy::import_note(&row, user, mappings)?;
            let actual = serde_json::to_value(legacy::note_json_model(&document, mappings)?)?;
            (document, serde_json::to_value(row)?, actual, None)
        } else {
            let row: crate::models::learning::Learning = serde_json::from_value(model.clone())?;
            let (document, usage) = legacy::import_learning(&row, user, mappings)?;
            let actual =
                serde_json::to_value(legacy::learning_json_model(&document, &usage, mappings)?)?;
            (document, serde_json::to_value(row)?, actual, Some(usage))
        };
        let unknown: Vec<_> = model
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !expected.as_object().unwrap().contains_key(*key))
            .collect();
        ensure!(unknown.is_empty(), "unrepresented SQL fields: {unknown:?}");
        let missing: Vec<_> = expected
            .as_object()
            .unwrap()
            .keys()
            .filter(|key| !model.as_object().unwrap().contains_key(*key))
            .collect();
        ensure!(missing.is_empty(), "missing SQL fields: {missing:?}");
        ensure!(expected == actual, "legacy model round trip differs");
        ensure!(
            legacy::legacy_user_id(&document, mappings)? == user,
            "SQL owner round trip differs"
        );
        let serialized = document.serialize()?;
        let parsed = super::document::Document::parse(&serialized)?;
        ensure!(parsed == document, "serialized document round trip differs");
        result.target_path = Some(
            Key::from_document(&document)?
                .relative_path()
                .to_string_lossy()
                .into_owned(),
        );
        result.document_digest = Some(digest(serialized.as_bytes()));
        extracted = Some((document, usage));
        Ok(())
    })();
    if let Err(e) = converted {
        result.issues.push(e.to_string());
    }
    (result, extracted)
}

pub async fn assess(pool: &PgPool, mappings: Option<&Mappings>) -> Result<Report> {
    visit(pool, mappings, |_, _, _, _| Ok(())).await
}

pub(crate) async fn visit(
    pool: &PgPool,
    mappings: Option<&Mappings>,
    visitor: impl FnMut(&Source, &Row, &super::document::Document, Option<&legacy::Usage>) -> Result<()>,
) -> Result<Report> {
    let mut tx = pool.begin().await?;
    let report = visit_transaction(&mut tx, mappings, visitor).await?;
    tx.rollback().await?;
    Ok(report)
}

/// A fresh transaction supplied by the publisher retains its generation lease
/// through filesystem publication. Uses the same connection even for size-one pools.
pub(crate) async fn visit_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    mappings: Option<&Mappings>,
    visitor: impl FnMut(&Source, &Row, &super::document::Document, Option<&legacy::Usage>) -> Result<()>,
) -> Result<Report> {
    // Guard row locks require a read/write-capable transaction, but this routine
    // issues no data mutations. RR pins one snapshot across both source tables.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL TIME ZONE 'UTC'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT public.ygg_knowledge_guard(false, $1, NULL)")
        .bind(CLIENT_PROTOCOL)
        .execute(&mut **tx)
        .await?;
    visit_on(tx, mappings, visitor).await
}

/// Caller retains a compatible generation lease for the whole scan. The owner
/// activation path uses this after its exclusive lease, including receipt-only
/// recovery after OKF was selected; ordinary readers use visit_transaction.
pub(crate) async fn visit_on(
    connection: &mut sqlx::PgConnection,
    mappings: Option<&Mappings>,
    mut visitor: impl FnMut(
        &Source,
        &Row,
        &super::document::Document,
        Option<&legacy::Usage>,
    ) -> Result<()>,
) -> Result<Report> {
    let source = sqlx::query_as::<_, Source>("SELECT database_id, generation, backend, corpus_id, pg_current_snapshot()::text AS snapshot FROM public.knowledge_storage WHERE singleton")
        .fetch_one(&mut *connection).await?;
    if let Some(mappings) = mappings {
        ensure!(
            mappings.database_id == source.database_id,
            "mapping file belongs to another source database"
        );
        ensure!(
            source.corpus_id.is_none_or(|id| id == mappings.corpus_id),
            "mapping file disagrees with fenced target corpus"
        );
    }
    let mut report = Report {
        version: 1,
        dry_run: true,
        source,
        mapping_digest: mappings
            .map(|m| serde_json::to_vec(m).map(|v| digest(&v)))
            .transpose()?,
        notes: 0,
        learnings: 0,
        owners: BTreeMap::new(),
        repositories: BTreeSet::new(),
        rows: Vec::new(),
        rows_verified: false,
    };
    for (table, query) in [
        (
            "memories",
            "SELECT to_jsonb(legacy_row.*) FROM public.memories AS legacy_row ORDER BY memory_id",
        ),
        (
            "learnings",
            "SELECT to_jsonb(legacy_row.*) FROM public.learnings AS legacy_row ORDER BY learning_id",
        ),
    ] {
        let mut stream = sqlx::query_scalar::<_, Value>(query).fetch(&mut *connection);
        while let Some(raw) = stream.try_next().await? {
            let (row, converted) = inspect(table, raw, mappings);
            if let Some((document, usage)) = &converted {
                visitor(&report.source, &row, document, usage.as_ref())?;
            }
            if let Some(user) = &row.user_id {
                *report.owners.entry(user.clone()).or_default() += 1;
            }
            if let Some(repo) = row.repo_id {
                report.repositories.insert(repo);
            }
            if table == "memories" {
                report.notes += 1;
            } else {
                report.learnings += 1;
            }
            report.rows.push(row);
        }
    }
    let mut ids = BTreeMap::<Uuid, usize>::new();
    for row in &report.rows {
        if let Some(id) = row.id {
            *ids.entry(id).or_default() += 1;
        }
    }
    for row in &mut report.rows {
        if row.id.is_some_and(|id| ids[&id] != 1) {
            row.issues
                .push("UUID occurs in multiple source rows".into());
        }
    }
    report.rows_verified = mappings.is_some() && report.rows.iter().all(|r| r.issues.is_empty());
    Ok(report)
}
