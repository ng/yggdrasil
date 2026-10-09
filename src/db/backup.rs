//! Consistent PostgreSQL dump component. Deployment publication must also include
//! knowledge/policy snapshots; this module never switches configuration.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

/// Deliberately redacted: native client connection settings contain credentials.
pub struct NativeConnection(BTreeMap<String, String>);
impl std::fmt::Debug for NativeConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeConnection([redacted])")
    }
}
impl NativeConnection {
    pub fn from_options(options: &PgConnectOptions) -> Result<Self> {
        // SQLx's URL builder interpolates username/host directly. Replace those
        // non-secret fields before serialization, then use effective getters.
        let encoded = options
            .clone()
            .username("ygg-tool")
            .host("localhost")
            .database("postgres")
            .to_url_lossy();
        let mut env = BTreeMap::new();
        env.insert(
            "PGHOST".into(),
            options
                .get_socket()
                .map(|path| {
                    path.to_str()
                        .ok_or_else(|| anyhow::anyhow!("native socket path must be UTF-8"))
                })
                .transpose()?
                .unwrap_or(options.get_host())
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
        );
        env.insert("PGPORT".into(), options.get_port().to_string());
        env.insert("PGUSER".into(), options.get_username().to_owned());
        // libpq treats the environment default as a literal database name;
        // unlike a --dbname argument, it is not expanded as connection info.
        env.insert(
            "PGDATABASE".into(),
            options
                .get_database()
                .unwrap_or(options.get_username())
                .to_owned(),
        );
        if let Some(password) = encoded.password() {
            // build_url percent-encodes every password delimiter, including '+'.
            let query = format!("value={password}");
            let decoded = url::form_urlencoded::parse(query.as_bytes())
                .next()
                .unwrap()
                .1
                .into_owned();
            env.insert("PGPASSWORD".into(), decoded);
        }
        for (key, value) in encoded.query_pairs() {
            let name = match key.as_ref() {
                "sslmode" => "PGSSLMODE",
                "sslrootcert" => "PGSSLROOTCERT",
                "sslcert" => "PGSSLCERT",
                "sslkey" => "PGSSLKEY",
                _ => continue,
            };
            ensure!(
                !value.contains("-----BEGIN"),
                "native tools require TLS certificate/key file paths"
            );
            let value = if name == "PGSSLMODE" {
                value.into_owned()
            } else {
                value
                    .strip_prefix("file: ")
                    .ok_or_else(|| {
                        anyhow::anyhow!("native tools require TLS certificate/key file paths")
                    })?
                    .to_owned()
            };
            env.insert(name.into(), value);
        }
        ensure!(
            !matches!(
                options.get_ssl_mode(),
                sqlx::postgres::PgSslMode::VerifyFull | sqlx::postgres::PgSslMode::VerifyCa
            ) || env.contains_key("PGSSLROOTCERT"),
            "native verified TLS requires an explicit sslrootcert file; SQLx and libpq default trust stores differ"
        );
        if let Some(value) = options.get_options() {
            env.insert("PGOPTIONS".into(), value.into());
        }
        env.insert("PGAPPNAME".into(), "ygg-backup".into());
        env.insert("PGCONNECT_TIMEOUT".into(), "10".into());
        // SQLx already resolved pgpass. Do not allow a later/default file or
        // libpq service configuration to silently change the selected identity.
        // A child of /dev/null cannot exist; libpq ignores failed stat without
        // warning, unlike /dev/null itself (not a regular password file).
        env.insert("PGPASSFILE".into(), "/dev/null/ygg-no-pgpass".into());
        env.insert("PGGSSENCMODE".into(), "disable".into());
        Ok(Self(env))
    }

    pub fn apply(&self, command: &mut Command) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PG") {
                command.env_remove(key);
            }
        }
        command
            .envs(&self.0)
            .stdin(Stdio::null())
            .kill_on_drop(true);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSnapshot {
    pub database_id: Uuid,
    pub generation: i64,
    pub backend: String,
    pub corpus_id: Option<Uuid>,
    pub server_major: i32,
    pub tool_version: String,
    pub migrations: Vec<i64>,
    pub table_rows: BTreeMap<String, i64>,
    pub bytes: u64,
    pub sha256: String,
}

async fn bounded(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(16385).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() <= 16384,
        "native tool diagnostic exceeded limit"
    );
    Ok(bytes)
}

/// All tool diagnostics stay out of errors, since servers can echo credentials.
/// Treat warnings as failure rather than certifying a potentially partial dump.
async fn run(mut command: Command, capture_stdout: bool) -> Result<Vec<u8>> {
    command.stderr(Stdio::piped()).kill_on_drop(true);
    if capture_stdout {
        command.stdout(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|_| anyhow::anyhow!("cannot launch PostgreSQL backup tool"))?;
    let mut stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take();
    let work = async {
        let (status, err, out) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            bounded(&mut stderr),
            async {
                match &mut stdout {
                    Some(stdout) => bounded(stdout).await,
                    None => Ok(Vec::new()),
                }
            }
        )?;
        ensure!(
            status.success() && err.is_empty(),
            "PostgreSQL backup tool failed or reported warnings; artifact is incomplete"
        );
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(1800), work)
        .await
        .map_err(|_| anyhow::anyhow!("PostgreSQL backup tool timed out; artifact is incomplete"))?
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Writes a custom-format archive through an already-open private file. Caller
/// owns staging/publication and must not publish on failure. No server lifecycle,
/// role creation, migration, destination database write or configuration change.
pub async fn dump(
    bin: &Path,
    options: &PgConnectOptions,
    output: &mut File,
) -> Result<DatabaseSnapshot> {
    tokio::time::timeout(Duration::from_secs(1800), dump_inner(bin, options, output))
        .await
        .map_err(|_| anyhow::anyhow!("database snapshot timed out; artifact is incomplete"))?
}

async fn dump_inner(
    bin: &Path,
    options: &PgConnectOptions,
    output: &mut File,
) -> Result<DatabaseSnapshot> {
    ensure!(
        bin.is_absolute(),
        "absolute PostgreSQL binary directory required"
    );
    let metadata = output.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0
            && metadata.len() == 0,
        "dump output must be an empty private regular file"
    );
    output.seek(SeekFrom::Start(0))?;
    let native = NativeConnection::from_options(options)?;
    let mut version = Command::new(bin.join("pg_dump"));
    native.apply(&mut version);
    version.arg("--version");
    let version = String::from_utf8(run(version, true).await?)
        .map_err(|_| anyhow::anyhow!("invalid pg_dump version"))?;
    ensure!(
        version.starts_with("pg_dump (PostgreSQL) "),
        "unexpected pg_dump version"
    );
    let tool_major: i32 = version
        .split_whitespace()
        .nth(2)
        .and_then(|v| v.split('.').next())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid pg_dump major version"))?;
    let mut connection = PgConnection::connect_with(options)
        .await
        .map_err(|_| anyhow::anyhow!("backup source connection failed"))?;
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut connection)
        .await?;
    let server_major: i32 =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::int / 10000")
            .fetch_one(&mut connection)
            .await?;
    ensure!(
        tool_major >= server_major,
        "pg_dump major is older than source server"
    );
    let (database_id, generation, backend, corpus_id): (Uuid, i64, String, Option<Uuid>) = sqlx::query_as("SELECT database_id, generation, backend, corpus_id FROM public.knowledge_storage WHERE singleton").fetch_one(&mut connection).await?;
    let migrations = sqlx::query_scalar(
        "SELECT version FROM public._sqlx_migrations WHERE success ORDER BY version",
    )
    .fetch_all(&mut connection)
    .await?;
    let tables: Vec<(String, String)> = sqlx::query_as("SELECT schemaname::text, tablename::text FROM pg_tables WHERE schemaname NOT IN ('pg_catalog', 'information_schema') AND schemaname !~ '^pg_toast' ORDER BY schemaname, tablename").fetch_all(&mut connection).await?;
    let mut table_rows = BTreeMap::new();
    for (schema, table) in tables {
        let name = format!("{}.{}", quote_identifier(&schema), quote_identifier(&table));
        let count = sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {name}"))
            .fetch_one(&mut connection)
            .await?;
        table_rows.insert(name, count);
    }
    let snapshot: String = sqlx::query_scalar("SELECT pg_export_snapshot()")
        .fetch_one(&mut connection)
        .await?;
    let mut command = Command::new(bin.join("pg_dump"));
    native.apply(&mut command);
    command
        .args(["--format=custom", "--no-password", "--snapshot", &snapshot])
        .stdout(Stdio::from(output.try_clone()?));
    run(command, false).await?;
    // Keep the exporting transaction open until pg_dump has fully exited.
    sqlx::query("ROLLBACK").execute(&mut connection).await?;
    connection.close().await?;
    output.sync_all()?;
    output.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count = output.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        hash.update(&buffer[..count]);
    }
    ensure!(bytes > 0, "empty database dump");
    Ok(DatabaseSnapshot {
        database_id,
        generation,
        backend,
        corpus_id,
        server_major,
        tool_version: version.trim().into(),
        migrations,
        table_rows,
        bytes,
        sha256: hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}
