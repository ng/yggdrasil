//! Restore only to an empty, quiesced database. Never clean or replace a target.
use super::backup::{self, DatabaseSnapshot, NativeConnection};
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub version: u32,
    pub table_sha256: BTreeMap<String, String>,
    pub schema: Vec<String>,
}

pub(crate) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Read within the caller's repeatable-read transaction. Sort canonical JSONB
/// rows with C collation and frame each row before hashing, preserving duplicate
/// rows, nulls and all columns (including IDs and migration checksums).
pub(crate) async fn evidence(
    connection: &mut PgConnection,
) -> Result<(Evidence, BTreeMap<String, i64>)> {
    sqlx::query("SET LOCAL search_path = pg_catalog")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET LOCAL timezone = 'UTC'")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET LOCAL extra_float_digits = 3")
        .execute(&mut *connection)
        .await?;
    let tables: Vec<(String, String)> = sqlx::query_as("SELECT schemaname::text, tablename::text FROM pg_tables WHERE schemaname NOT IN ('pg_catalog', 'information_schema') AND schemaname !~ '^pg_toast' ORDER BY schemaname, tablename").fetch_all(&mut *connection).await?;
    let mut table_rows = BTreeMap::new();
    let mut table_sha256 = BTreeMap::new();
    for (schema, table) in tables {
        let name = format!("{}.{}", identifier(&schema), identifier(&table));
        let sql = format!("SELECT to_jsonb(t)::text COLLATE \"C\" FROM {name} AS t ORDER BY 1");
        let mut rows = sqlx::query_scalar::<_, String>(&sql).fetch(&mut *connection);
        let mut hash = Sha256::new();
        let mut count = 0i64;
        while let Some(row) = rows.try_next().await? {
            hash.update((row.len() as u64).to_be_bytes());
            hash.update(row.as_bytes());
            count += 1;
        }
        table_rows.insert(name.clone(), count);
        table_sha256.insert(
            name,
            hash.finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
    }
    let schema = schema_evidence(connection).await?;
    Ok((
        Evidence {
            version: 1,
            table_sha256,
            schema,
        },
        table_rows,
    ))
}

/// Catalog definitions recorded by backups. Ownership/ACL rebinding remains a
/// separate deployment concern; compare on the same source before knowledge cutover.
pub(crate) async fn schema_evidence(connection: &mut PgConnection) -> Result<Vec<String>> {
    sqlx::query("SET LOCAL search_path = pg_catalog")
        .execute(&mut *connection)
        .await?;
    // Catalog identities exclude OIDs, role ownership and ACLs, which must be
    // rebound to the destination owner/runtime roles. Definitions are deparsed
    // with a fixed search_path. Unknown version differences fail comparison.
    let schema = sqlx::query_scalar::<_, String>(r#"
WITH user_ns AS (SELECT oid, nspname FROM pg_namespace WHERE nspname NOT IN ('pg_catalog', 'information_schema') AND nspname !~ '^pg_toast' AND nspname !~ '^pg_temp'),
objects AS (
 SELECT jsonb_build_array('relation', n.nspname, c.relname, c.relkind, c.relpersistence, c.relrowsecurity, c.relforcerowsecurity)::text AS definition FROM pg_class c JOIN user_ns n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','v','m','S','f')
 UNION ALL SELECT jsonb_build_array('column', n.nspname, c.relname, dense_rank() OVER (PARTITION BY c.oid ORDER BY a.attnum), a.attname, format_type(a.atttypid,a.atttypmod), a.attnotnull, a.attidentity, a.attgenerated, (SELECT jsonb_build_array(nc.nspname, co.collname) FROM pg_collation co JOIN pg_namespace nc ON nc.oid=co.collnamespace WHERE co.oid=a.attcollation), pg_get_expr(d.adbin,d.adrelid))::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN user_ns n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attnum>0 AND NOT a.attisdropped AND c.relkind IN ('r','p','v','m','f')
 UNION ALL SELECT jsonb_build_array('constraint', n.nspname, c.conname, c.conrelid::regclass::text, c.convalidated, pg_get_constraintdef(c.oid))::text FROM pg_constraint c JOIN user_ns n ON n.oid=c.connamespace
 UNION ALL SELECT jsonb_build_array('index', n.nspname, c.relname, i.indisvalid, i.indisready, pg_get_indexdef(i.indexrelid))::text FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN user_ns n ON n.oid=c.relnamespace
 UNION ALL SELECT jsonb_build_array('trigger', n.nspname, c.relname, t.tgname, t.tgenabled, pg_get_triggerdef(t.oid))::text FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN user_ns n ON n.oid=c.relnamespace WHERE NOT t.tgisinternal
 UNION ALL SELECT jsonb_build_array('enum', n.nspname, t.typname, e.enumsortorder, e.enumlabel)::text FROM pg_enum e JOIN pg_type t ON t.oid=e.enumtypid JOIN user_ns n ON n.oid=t.typnamespace
 UNION ALL SELECT jsonb_build_array('routine', n.nspname, p.proname, pg_get_function_identity_arguments(p.oid), pg_get_functiondef(p.oid))::text FROM pg_proc p JOIN user_ns n ON n.oid=p.pronamespace WHERE p.prokind IN ('f','p') AND NOT EXISTS(SELECT 1 FROM pg_depend d WHERE d.classid='pg_proc'::regclass AND d.objid=p.oid AND d.deptype='e')
 UNION ALL SELECT jsonb_build_array('view', n.nspname, c.relname, pg_get_viewdef(c.oid))::text FROM pg_class c JOIN user_ns n ON n.oid=c.relnamespace WHERE c.relkind IN ('v','m')
 UNION ALL SELECT jsonb_build_array('database', pg_encoding_to_char(d.encoding), d.datcollate, d.datctype, to_jsonb(d)->>'datlocprovider', COALESCE(to_jsonb(d)->>'datlocale', to_jsonb(d)->>'daticulocale'))::text FROM pg_database d WHERE d.datname=current_database()
 UNION ALL SELECT jsonb_build_array('extension', e.extname, e.extversion, n.nspname)::text FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace
)
SELECT definition FROM objects ORDER BY definition COLLATE "C"
"#).fetch_all(&mut *connection).await?;
    Ok(schema)
}

/// Same canonical, length-framed row digest as the source backup. Call within a
/// transaction that pins or fences the requested table and uses UTC formatting.
pub(crate) async fn table_evidence(
    connection: &mut PgConnection,
    schema: &str,
    table: &str,
) -> Result<(i64, String)> {
    sqlx::query("SET LOCAL timezone = 'UTC'")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET LOCAL extra_float_digits = 3")
        .execute(&mut *connection)
        .await?;
    let name = format!("{}.{}", identifier(schema), identifier(table));
    let query = format!("SELECT to_jsonb(t)::text COLLATE \"C\" FROM {name} AS t ORDER BY 1");
    let mut rows = sqlx::query_scalar::<_, String>(&query).fetch(connection);
    let mut hash = Sha256::new();
    let mut count = 0;
    while let Some(row) = rows.try_next().await? {
        hash.update((row.len() as u64).to_be_bytes());
        hash.update(row.as_bytes());
        count += 1;
    }
    Ok((
        count,
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    ))
}

// Reuse the database tuple in version-1 schema evidence. A missing or ambiguous
// tuple cannot establish compatibility; never infer defaults from the target.
fn database_properties(evidence: &Evidence) -> Result<serde_json::Value> {
    let mut database = None;
    for definition in &evidence.schema {
        let value: serde_json::Value = serde_json::from_str(definition)?;
        if value.get(0).and_then(serde_json::Value::as_str) != Some("database") {
            continue;
        }
        ensure!(
            database.is_none(),
            "backup has duplicate database encoding/locale evidence"
        );
        let fields = value.as_array().unwrap();
        ensure!(
            fields.len() == 6
                && fields[1..4].iter().all(serde_json::Value::is_string)
                && fields[4..].iter().all(|v| v.is_null() || v.is_string()),
            "backup has unsupported database encoding/locale evidence"
        );
        database = Some(value);
    }
    database.ok_or_else(|| {
        anyhow::anyhow!("backup lacks database encoding/locale evidence; create a new backup")
    })
}

async fn require_database_properties(
    connection: &mut PgConnection,
    expected: &serde_json::Value,
) -> Result<()> {
    let actual: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_array('database', pg_encoding_to_char(d.encoding), d.datcollate, d.datctype, to_jsonb(d)->>'datlocprovider', COALESCE(to_jsonb(d)->>'datlocale', to_jsonb(d)->>'daticulocale')) FROM pg_database d WHERE d.datname=current_database()",
    ).fetch_one(connection).await?;
    ensure!(
        actual == *expected,
        "restore target database encoding/locale differs from backup; create a matching empty target before restoring"
    );
    Ok(())
}

/// PG18 records table NOT NULL constraints separately; PG16/17 recorded the
/// same requirement in attnotnull. Keep archived evidence and same-major checks
/// exact. For this one forward boundary, omit only ordinary target constraints
/// whose column was already NOT NULL in the source. Domain constraints and
/// newer validation/enforcement/inheritance semantics are never omitted.
async fn restore_evidence_matches(
    connection: &mut PgConnection,
    mut actual: Evidence,
    expected: &Evidence,
    source_major: i32,
) -> Result<bool> {
    let target_major: i32 =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::int / 10000")
            .fetch_one(&mut *connection)
            .await?;
    if matches!(source_major, 16 | 17) && target_major == 18 {
        let candidates: Vec<(String, String, String, String)> = sqlx::query_as(
            r#"
SELECT jsonb_build_array('constraint', n.nspname, c.conname, c.conrelid::regclass::text,
                         c.convalidated, pg_get_constraintdef(c.oid))::text,
       n.nspname::text, r.relname::text, a.attname::text
FROM pg_constraint c
JOIN pg_namespace n ON n.oid=c.connamespace
JOIN pg_class r ON r.oid=c.conrelid
JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=c.conkey[1]
WHERE c.contype='n' AND c.contypid=0 AND r.relkind='r'
  AND cardinality(c.conkey)=1 AND a.attnotnull AND NOT a.attisdropped
  AND c.convalidated AND c.conenforced AND c.conislocal
  AND NOT c.condeferrable AND NOT c.condeferred AND NOT c.connoinherit
  AND c.coninhcount=0 AND c.conparentid=0
  AND n.nspname NOT IN ('pg_catalog','information_schema')
  AND n.nspname !~ '^pg_toast' AND n.nspname !~ '^pg_temp'
"#,
        )
        .fetch_all(&mut *connection)
        .await?;
        let source_columns: std::collections::BTreeSet<(String, String, String)> = expected
            .schema
            .iter()
            .map(|definition| serde_json::from_str::<serde_json::Value>(definition))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|value| {
                let fields = value.as_array()?;
                if fields.len() != 11 || fields[0] != "column" || fields[6] != true {
                    return None;
                }
                Some((
                    fields[1].as_str()?.into(),
                    fields[2].as_str()?.into(),
                    fields[4].as_str()?.into(),
                ))
            })
            .collect();
        let mut omitted_columns = std::collections::BTreeSet::new();
        for (definition, schema, table, column) in candidates {
            let column = (schema, table, column);
            if source_columns.contains(&column) && !expected.schema.contains(&definition) {
                ensure!(
                    omitted_columns.insert(column),
                    "ambiguous restored NOT NULL constraints"
                );
                let Some(position) = actual.schema.iter().position(|entry| entry == &definition)
                else {
                    anyhow::bail!("restored constraint catalog changed during validation");
                };
                actual.schema.remove(position);
            }
        }
    }
    Ok(actual == *expected)
}

pub async fn validate(options: &PgConnectOptions, expected: &DatabaseSnapshot) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(1800), async {
        let mut connection = PgConnection::connect_with(options).await.map_err(|_| anyhow::anyhow!("restore validation connection failed"))?;
        sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut connection).await?;
        let marker: (uuid::Uuid, i64, String, Option<uuid::Uuid>) = sqlx::query_as("SELECT database_id, generation, backend, corpus_id FROM public.knowledge_storage WHERE singleton").fetch_one(&mut connection).await?;
        ensure!(marker == (expected.database_id, expected.generation, expected.backend.clone(), expected.corpus_id), "restored database identity or storage binding differs");
        let migrations: Vec<i64> = sqlx::query_scalar("SELECT version FROM public._sqlx_migrations WHERE success ORDER BY version").fetch_all(&mut connection).await?;
        ensure!(migrations == expected.migrations, "restored migration versions differ");
        let (actual, counts) = evidence(&mut connection).await?;
        ensure!(counts == expected.table_rows, "restored table inventory or counts differ");
        let source = expected.validation.as_ref().ok_or_else(|| anyhow::anyhow!("backup lacks restore evidence"))?;
        ensure!(restore_evidence_matches(&mut connection, actual, source, expected.server_major).await?, "restored content or schema differs from backup");
        sqlx::query("ROLLBACK").execute(&mut connection).await?;
        connection.close().await?;
        Ok(())
    }).await.map_err(|_| anyhow::anyhow!("restore validation timed out"))?
}

/// The operator must keep the empty target isolated from other writers until
/// validation succeeds. A session lease excludes other cooperating restores.
/// Failure retains the target, never retries a possibly committed restore and
/// never changes configuration. The dump is trusted SQL, not a sandboxed format.
pub async fn database(
    bin: &Path,
    options: &PgConnectOptions,
    archive: &mut File,
    expected: &DatabaseSnapshot,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(1800), async {
        ensure!(bin.is_absolute(), "absolute PostgreSQL binary directory required");
        ensure!(expected.validation.as_ref().is_some_and(|v| v.version == 1), "backup lacks supported restore evidence; create a new backup");
        let properties = database_properties(expected.validation.as_ref().unwrap())?;
        let meta = archive.metadata()?;
        ensure!(meta.is_file() && meta.nlink() == 1, "restore archive must be a regular non-hardlinked file");
        archive.seek(SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut bytes = 0u64;
        loop { let n = archive.read(&mut buffer)?; if n == 0 { break; } hash.update(&buffer[..n]); bytes += n as u64; }
        ensure!(bytes == expected.bytes && hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>() == expected.sha256, "restore archive integrity mismatch");
        archive.seek(SeekFrom::Start(0))?;
        let native = NativeConnection::from_options(options)?;
        let mut version = Command::new(bin.join("pg_restore"));
        native.apply(&mut version);
        version.arg("--version");
        let version = String::from_utf8(backup::run(version, true).await?)?;
        ensure!(version.starts_with("pg_restore (PostgreSQL) "), "unexpected pg_restore version");
        let tool_major: i32 = version.split_whitespace().nth(2).and_then(|v| v.split('.').next()).and_then(|v|v.parse().ok()).ok_or_else(||anyhow::anyhow!("invalid pg_restore version"))?;
        let mut lease = PgConnection::connect_with(options).await.map_err(|_|anyhow::anyhow!("restore target connection failed"))?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(1497843531, 3)").fetch_one(&mut lease).await?;
        ensure!(locked, "another restore holds the target lease");
        let major: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int / 10000").fetch_one(&mut lease).await?;
        ensure!(major >= expected.server_major && tool_major >= expected.server_major, "restore cannot downgrade PostgreSQL major version");
        require_database_properties(&mut lease, &properties).await?;
        let empty: bool = sqlx::query_scalar(r#"SELECT NOT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname NOT IN ('pg_catalog','information_schema','public') AND nspname !~ '^pg_toast' AND nspname !~ '^pg_temp') AND NOT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public') AND NOT EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public') AND NOT EXISTS(SELECT 1 FROM pg_type t JOIN pg_namespace n ON n.oid=t.typnamespace WHERE n.nspname='public') AND NOT EXISTS(SELECT 1 FROM pg_extension WHERE extname <> 'plpgsql')"#).fetch_one(&mut lease).await?;
        ensure!(empty, "restore target must be empty; existing objects are never removed");
        let others: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND pid<>pg_backend_pid()").fetch_one(&mut lease).await?;
        ensure!(others == 0, "restore target has other sessions; quiesce it first");
        let mut command = Command::new(bin.join("pg_restore"));
        native.apply(&mut command);
        // --dbname is a conninfo override; PGDATABASE stays a literal environment
        // default, keeping even names containing '=' or quotes out of conninfo.
        command.args(["--dbname", "application_name=ygg-restore", "--no-password", "--no-owner", "--no-acl", "--exit-on-error", "--single-transaction"])
            .stdin(Stdio::from(archive.try_clone()?)).stdout(Stdio::null());
        tokio::select! {
            result = async {
                backup::run(command, false).await?;
                validate(options, expected).await
            } => result?,
            result = async {
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let held = tokio::time::timeout(Duration::from_secs(3), sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND locktype='advisory' AND classid=1497843531 AND objid=3 AND objsubid=2 AND granted)").fetch_one(&mut lease))
                        .await.map_err(|_| anyhow::anyhow!("restore lease timed out; target outcome unknown"))?
                        .map_err(|_| anyhow::anyhow!("restore lease connection lost; target outcome unknown"))?;
                    ensure!(held, "restore lease lost; target outcome unknown");
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            } => result?,
        }
        lease.close().await?;
        Ok(())
    }).await.map_err(|_|anyhow::anyhow!("restore timed out; target retained, inspect before retrying"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pg18_restore_compatibility_keeps_other_schema_and_row_checks() {
        let options =
            crate::db::external::options(&std::env::var("DATABASE_URL").unwrap()).unwrap();
        let mut connection = PgConnection::connect_with(&options).await.unwrap();
        let major: i32 =
            sqlx::query_scalar("SELECT current_setting('server_version_num')::int / 10000")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        if major != 18 {
            return; // The standard CI matrix includes PG18 and exercises this case.
        }
        // Transactional fixture: even a panic drops the connection and rolls back.
        sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut connection)
            .await
            .unwrap();
        let schema = format!("restore_{}", uuid::Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema};
             CREATE DOMAIN {schema}.positive AS int CONSTRAINT positive_check CHECK (VALUE > 0);
             CREATE TABLE {schema}.items (id int PRIMARY KEY, required text CONSTRAINT required_nn NOT NULL,
                 optional text, value {schema}.positive, CONSTRAINT required_check CHECK (required <> ''));
             INSERT INTO {schema}.items VALUES (1, 'preserved', 'present', 1)"
        )).execute(&mut connection).await.unwrap();
        let (original, _) = evidence(&mut connection).await.unwrap();
        let added: Vec<String> = sqlx::query_scalar(
            "SELECT jsonb_build_array('constraint', n.nspname, c.conname, c.conrelid::regclass::text, c.convalidated, pg_get_constraintdef(c.oid))::text FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname=$1 AND c.contype='n' AND c.conrelid<>0"
        ).bind(&schema).fetch_all(&mut connection).await.unwrap();
        assert_eq!(added.len(), 2);
        let mut legacy = original.clone();
        legacy.schema.retain(|entry| !added.contains(entry));
        for source_major in [16, 17] {
            assert!(
                restore_evidence_matches(&mut connection, original.clone(), &legacy, source_major)
                    .await
                    .unwrap()
            );
        }
        for source_major in [15, 18, 19] {
            assert!(
                !restore_evidence_matches(&mut connection, original.clone(), &legacy, source_major)
                    .await
                    .unwrap()
            );
        }
        assert!(
            restore_evidence_matches(&mut connection, original.clone(), &original, 18)
                .await
                .unwrap()
        );
        for change in [
            "ALTER TABLE SCHEMA.items DROP CONSTRAINT required_check",
            "ALTER DOMAIN SCHEMA.positive DROP CONSTRAINT positive_check",
            "ALTER DOMAIN SCHEMA.positive SET NOT NULL",
            "ALTER TABLE SCHEMA.items DROP CONSTRAINT items_pkey",
            "ALTER TABLE SCHEMA.items ALTER COLUMN required DROP NOT NULL",
            "ALTER TABLE SCHEMA.items ALTER COLUMN optional SET NOT NULL",
            "ALTER TABLE SCHEMA.items DROP CONSTRAINT required_nn; ALTER TABLE SCHEMA.items ADD CONSTRAINT required_nn NOT NULL required NOT VALID",
            "ALTER TABLE SCHEMA.items DROP CONSTRAINT required_nn; ALTER TABLE SCHEMA.items ADD CONSTRAINT required_nn NOT NULL required NO INHERIT",
            "UPDATE SCHEMA.items SET required='changed'",
        ] {
            sqlx::query("SAVEPOINT mutation")
                .execute(&mut connection)
                .await
                .unwrap();
            sqlx::raw_sql(&change.replace("SCHEMA", &schema))
                .execute(&mut connection)
                .await
                .unwrap();
            let (changed, _) = evidence(&mut connection).await.unwrap();
            assert!(
                !restore_evidence_matches(&mut connection, changed, &legacy, 16)
                    .await
                    .unwrap(),
                "accepted {change}"
            );
            sqlx::query("ROLLBACK TO SAVEPOINT mutation")
                .execute(&mut connection)
                .await
                .unwrap();
        }
        sqlx::query(&format!(
            "ALTER TABLE {schema}.items RENAME CONSTRAINT required_nn TO renamed_nn"
        ))
        .execute(&mut connection)
        .await
        .unwrap();
        let (renamed, _) = evidence(&mut connection).await.unwrap();
        assert!(
            !restore_evidence_matches(&mut connection, renamed, &original, 18)
                .await
                .unwrap()
        );
        sqlx::query("ROLLBACK")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
    }

    #[test]
    fn restore_requires_one_complete_database_properties_record() {
        let property = serde_json::json!(["database", "UTF8", "C", "C", "c", null]);
        let mut evidence = Evidence {
            version: 1,
            table_sha256: BTreeMap::new(),
            schema: vec![],
        };
        assert!(database_properties(&evidence).is_err());
        evidence.schema = vec![property.to_string()];
        assert_eq!(database_properties(&evidence).unwrap(), property);
        evidence.schema.push(property.to_string());
        assert!(database_properties(&evidence).is_err());
        for malformed in [
            serde_json::json!(["database", "UTF8"]),
            serde_json::json!(["database", "UTF8", "C", "C", 3, null]),
        ] {
            evidence.schema = vec![malformed.to_string()];
            assert!(database_properties(&evidence).is_err());
        }
    }
}
