//! Integrity-bound source backup and recorded schema/legacy-row parity. These
//! checks do not stop external schema operators; maintenance must quiesce them.
use crate::db::{deployment_backup, restore};
use anyhow::{Result, ensure};
use sqlx::PgConnection;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct SourceBackup {
    path: PathBuf,
    digest: String,
    manifest: deployment_backup::Manifest,
}
impl SourceBackup {
    pub fn open(path: &Path, database: Uuid, generation: i64) -> Result<Self> {
        Self::open_source(path, database, generation, None)
    }
    pub(crate) fn open_okf(
        path: &Path,
        database: Uuid,
        generation: i64,
        corpus: Uuid,
    ) -> Result<Self> {
        Self::open_source(path, database, generation, Some(corpus))
    }
    fn open_source(
        path: &Path,
        database: Uuid,
        generation: i64,
        corpus: Option<Uuid>,
    ) -> Result<Self> {
        ensure!(path.is_absolute(), "absolute source backup path required");
        let path = path.canonicalize()?;
        let manifest = deployment_backup::verify(&path)?;
        ensure!(
            manifest.database.database_id == database
                && manifest.database.generation == generation
                && generation > 0
                && manifest.database.backend == if corpus.is_some() { "okf" } else { "sql" }
                && manifest.database.corpus_id == corpus
                && corpus.is_none_or(|id| manifest
                    .knowledge
                    .as_ref()
                    .is_some_and(|k| k.corpus_id == id)),
            "source backup must be the expected database generation and corpus"
        );
        let evidence = manifest
            .database
            .validation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("source backup lacks schema/content evidence"))?;
        ensure!(
            evidence.version == 1 && !evidence.schema.is_empty(),
            "source backup lacks supported schema evidence"
        );
        let digest = super::document::digest(&serde_json::to_vec(&manifest)?);
        Ok(Self {
            path,
            digest,
            manifest,
        })
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn verify(&self) -> Result<()> {
        let current = deployment_backup::verify(&self.path)?;
        ensure!(
            super::document::digest(&serde_json::to_vec(&current)?) == self.digest,
            "source backup changed after migration preparation"
        );
        if let Some(configuration) = deployment_backup::read_configuration(&self.path, &current)? {
            configuration.verify_sources()?;
        }
        Ok(())
    }
    pub(crate) fn verify_configuration(
        &self,
        config: &crate::config::database::DeploymentConfig,
    ) -> Result<()> {
        if let Some(configuration) =
            deployment_backup::read_configuration(&self.path, &self.manifest)?
        {
            configuration.verify_selection(config)?;
        }
        Ok(())
    }
    /// Caller holds the migration lease before changing the storage marker.
    /// Coordination/telemetry rows may change; legacy knowledge and migration
    /// checksums must remain exactly the rows captured by the consistent dump.
    pub async fn verify_on(&self, connection: &mut PgConnection) -> Result<()> {
        self.verify_schema_on(connection).await?;
        self.verify_tables(connection, &["memories", "learnings"])
            .await
    }
    /// Reverse activation changes legacy rows; retain the backup's schema and
    /// migration-version checks without mistaking restored/current rows for drift.
    pub(crate) async fn verify_schema_on(&self, connection: &mut PgConnection) -> Result<()> {
        self.verify()?;
        verify_guards(connection).await?;
        sqlx::query("LOCK TABLE public.memories, public.learnings, public._sqlx_migrations IN ACCESS SHARE MODE").execute(&mut *connection).await?;
        let database: Uuid =
            sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage WHERE singleton")
                .fetch_one(&mut *connection)
                .await?;
        ensure!(
            database == self.manifest.database.database_id,
            "source backup database identity differs"
        );
        let expected = self.manifest.database.validation.as_ref().unwrap();
        ensure!(
            restore::schema_evidence(connection).await? == expected.schema,
            "database schema changed since source backup"
        );
        self.verify_tables(connection, &["_sqlx_migrations"]).await
    }
    async fn verify_tables(&self, connection: &mut PgConnection, tables: &[&str]) -> Result<()> {
        let expected = self.manifest.database.validation.as_ref().unwrap();
        for table in tables {
            let key = format!("\"public\".\"{table}\"");
            let (count, hash) = restore::table_evidence(connection, "public", table).await?;
            ensure!(
                self.manifest.database.table_rows.get(&key) == Some(&count)
                    && expected.table_sha256.get(&key) == Some(&hash),
                "source {table} changed since backup; retain evidence and prepare a fresh maintenance operation"
            );
        }
        Ok(())
    }
}

/// Backup equality alone would accept a source with an already-disabled fence.
/// Check the installed trigger definitions and bodies against shipped migrations.
pub(crate) async fn verify_guards(connection: &mut PgConnection) -> Result<()> {
    const ORIGINAL: &str =
        include_str!("../../migrations/20261008000002_knowledge_storage_guard.sql");
    const REVERSE: &str =
        include_str!("../../migrations/20261009000001_knowledge_reverse_import.sql");
    for (table, trigger, function, flags, migration, definer, path) in [
        (
            "memories",
            "ygg_knowledge_write_fence",
            "ygg_knowledge_write_fence",
            62i16,
            REVERSE,
            true,
            "search_path=pg_catalog, pg_temp",
        ),
        (
            "learnings",
            "ygg_knowledge_write_fence",
            "ygg_knowledge_write_fence",
            62,
            REVERSE,
            true,
            "search_path=pg_catalog, pg_temp",
        ),
        (
            "knowledge_storage",
            "ygg_knowledge_marker_lease",
            "ygg_knowledge_marker_lease",
            58,
            ORIGINAL,
            false,
            "search_path=pg_catalog, public",
        ),
        (
            "knowledge_storage",
            "ygg_knowledge_marker_change",
            "ygg_knowledge_marker_change",
            27,
            ORIGINAL,
            false,
            "search_path=pg_catalog, public",
        ),
        (
            "knowledge_storage",
            "ygg_knowledge_marker_truncate",
            "ygg_knowledge_marker_change",
            34,
            ORIGINAL,
            false,
            "search_path=pg_catalog, public",
        ),
    ] {
        let prefix = format!("FUNCTION public.{function}(");
        let body = migration
            .split_once(&prefix)
            .and_then(|(_, s)| s.split_once("$$"))
            .and_then(|(_, s)| s.split_once("$$"))
            .map(|(s, _)| s)
            .expect("shipped guard body");
        let row: Option<(i16,String,String,bool,Option<Vec<String>>,bool)> = sqlx::query_as(
            "SELECT t.tgtype,t.tgenabled::text,p.prosrc,p.prosecdef,p.proconfig,t.tgqual IS NULL AND t.tgnargs=0 AND NOT t.tgisinternal FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_proc p ON p.oid=t.tgfoid JOIN pg_catalog.pg_namespace pn ON pn.oid=p.pronamespace WHERE n.nspname='public' AND c.relname=$1 AND t.tgname=$2 AND pn.nspname='public' AND p.proname=$3")
            .bind(table).bind(trigger).bind(function).fetch_optional(&mut *connection).await?;
        ensure!(
            row.is_some_and(|r| r.0 == flags
                && r.1 == "O"
                && r.2 == body
                && r.3 == definer
                && r.4 == Some(vec![path.into()])
                && r.5),
            "required knowledge guard {table}.{trigger} is missing or modified"
        );
    }
    Ok(())
}
