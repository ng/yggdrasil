#![cfg(any(target_os = "macos", target_os = "linux"))]
//! Requires a disposable PostgreSQL cluster with database-creation privileges.
use futures::FutureExt;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, process::Command};
use uuid::Uuid;
use ygg::knowledge::{inventory, legacy::Mappings};

struct Fixture {
    admin: PgPool,
    pool: PgPool,
    database: String,
    url: String,
}
impl Fixture {
    async fn new() -> Self {
        let base = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&base)
            .await
            .unwrap();
        let database = format!("ygg_inventory_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&base).unwrap();
        url.set_path(&format!("/{database}"));
        let url = url.to_string();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        ygg::db::run_migrations(&pool).await.unwrap();
        Self {
            admin,
            pool,
            database,
            url,
        }
    }
    async fn cleanup(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {}", self.database))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
async fn inventory_cli_and_staging_preserve_source_and_detect_conflicts() {
    let f = Fixture::new().await;
    let result = std::panic::AssertUnwindSafe(async {
    let note = Uuid::new_v4();
    let rule = Uuid::new_v4();
    sqlx::query("INSERT INTO memories(memory_id,text,user_id) VALUES($1,'note λ','alice')")
        .bind(note)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO learnings(learning_id,text,context,status,source,user_id,applied_count,scope_tags,approved_at) VALUES($1,E'rule λ\\n','','pending','proposed','',7,'[1,null]'::jsonb,'2026-01-01T00:00:00Z')")
        .bind(rule).execute(&f.pool).await.unwrap();
    let before = inventory::assess(&f.pool, None).await.unwrap();
    assert_eq!((before.notes, before.learnings), (1, 1));
    assert!(!before.rows_verified);
    assert!(before.rows.iter().all(|row| !row.issues.is_empty()));
    assert_eq!(
        before.owners,
        BTreeMap::from([("".into(), 1), ("alice".into(), 1)])
    );
    let mut mappings = Mappings {
        database_id: before.source.database_id,
        corpus_id: Uuid::new_v4(),
        repos: BTreeMap::new(),
        users: BTreeMap::from([
            ("".into(), "owner-empty".into()),
            ("alice".into(), "owner-alice".into()),
        ]),
    };
    let report = inventory::assess(&f.pool, Some(&mappings)).await.unwrap();
    assert!(report.rows_verified);
    assert!(report.dry_run);
    assert!(
        report
            .rows
            .iter()
            .all(|row| row.target_path.is_some() && row.document_digest.is_some())
    );
    assert_eq!(
        before
            .rows
            .iter()
            .map(|r| &r.source_digest)
            .collect::<Vec<_>>(),
        report
            .rows
            .iter()
            .map(|r| &r.source_digest)
            .collect::<Vec<_>>()
    );
    let temp = tempfile::tempdir().unwrap();
    let mapping_path = temp.path().join("mapping.json");
    std::fs::write(&mapping_path, serde_json::to_vec(&mappings).unwrap()).unwrap();
    let knowledge = temp.path().join("must-not-exist");
    let staging = temp.path().join("staging");
    assert!(ygg::knowledge::export::stage(&f.pool, &mappings, &staging).await.is_err());
    assert!(!staging.exists());
    let cli = |mapped: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ygg"));
        command
            .args(["knowledge", "migrate", "--dry-run", "--json"])
            .env("DATABASE_URL", &f.url)
            .env("YGG_KNOWLEDGE_DIR", &knowledge);
        if mapped {
            command.arg("--mapping-file").arg(&mapping_path);
        }
        command.output().unwrap()
    };
    let unresolved = cli(false);
    assert!(!unresolved.status.success());
    let json: serde_json::Value = serde_json::from_slice(&unresolved.stdout).unwrap();
    assert_eq!(json["rows_verified"], false);
    let verified = cli(true);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&verified.stdout).unwrap();
    assert_eq!(json["rows_verified"], true);
    assert_eq!(json["rows"].as_array().unwrap().len(), 2);
    assert!(!knowledge.exists());
    let marker: (String, i64) = sqlx::query_as("SELECT backend,generation FROM knowledge_storage")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(marker, ("sql".into(), 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT applied_count::bigint FROM learnings WHERE learning_id=$1"
        )
        .bind(rule)
        .fetch_one(&f.pool)
        .await
        .unwrap(),
        7
    );
    mappings.database_id = Uuid::new_v4();
    assert!(inventory::assess(&f.pool, Some(&mappings)).await.is_err());
    mappings.database_id = before.source.database_id;
    sqlx::query("ALTER TABLE memories ADD COLUMN future_field TEXT")
        .execute(&f.pool)
        .await
        .unwrap();
    let changed = inventory::assess(&f.pool, Some(&mappings)).await.unwrap();
    assert!(!changed.rows_verified);
    assert!(
        changed
            .rows
            .iter()
            .find(|r| r.id == Some(note))
            .unwrap()
            .issues
            .iter()
            .any(|i| i.contains("future_field"))
    );
    sqlx::query("ALTER TABLE memories DROP COLUMN future_field")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO learnings(learning_id,text,user_id) VALUES($1,'duplicate','alice')")
        .bind(note)
        .execute(&f.pool)
        .await
        .unwrap();
    let duplicates = inventory::assess(&f.pool, Some(&mappings)).await.unwrap();
    assert!(!duplicates.rows_verified);
    assert_eq!(
        duplicates
            .rows
            .iter()
            .filter(|r| r.issues.iter().any(|i| i.contains("multiple source rows")))
            .count(),
        2
    );
    sqlx::query("DELETE FROM learnings WHERE learning_id=$1").bind(note).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE knowledge_storage SET backend='fenced', generation=2, corpus_id=$1")
        .bind(mappings.corpus_id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        inventory::assess(&f.pool, Some(&mappings))
            .await
            .unwrap()
            .source
            .backend,
        "fenced"
    );
    let unrelated = temp.path().join("unrelated");
    drop(ygg::knowledge::store::KnowledgeStore::open(&unrelated, true).unwrap());
    std::fs::write(unrelated.join("keep.txt"), "keep").unwrap();
    assert!(ygg::knowledge::export::stage(&f.pool, &mappings, &unrelated).await.is_err());
    assert_eq!(std::fs::read_to_string(unrelated.join("keep.txt")).unwrap(), "keep");
    let empty = temp.path().join("empty-stage");
    drop(ygg::knowledge::store::KnowledgeStore::open(&empty, true).unwrap());
    let empty_manifest = ygg::knowledge::export::stage(&f.pool, &mappings, &empty).await.unwrap();
    let manifest = ygg::knowledge::export::stage(&f.pool, &mappings, &staging).await.unwrap();
    assert_eq!(manifest.entries.len(), 2);
    assert_eq!(ygg::knowledge::export::verify(&staging).unwrap(), manifest);
    assert_eq!(ygg::knowledge::export::stage(&f.pool, &mappings, &staging).await.unwrap(), manifest);
    // Publish on a one-connection pool: inventory and generation lease must use
    // the same transaction, and retries may not overwrite independent edits.
    let single = PgPoolOptions::new().max_connections(1).connect(&f.url).await.unwrap();
    std::fs::write(empty.join("unlisted.txt"), b"must not publish").unwrap();
    let unlisted_target = temp.path().join("unlisted-target");
    assert!(ygg::knowledge::export::publish(&single, &empty,
        &temp.path().join("unlisted-archive"), &unlisted_target).await.is_err());
    assert!(!unlisted_target.exists());
    std::fs::remove_file(empty.join("unlisted.txt")).unwrap();
    let archive = temp.path().join("publication-archive");
    let published = temp.path().join("published");
    assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(10),
        ygg::knowledge::export::publish(&single, &empty, &archive, &published))
        .await.unwrap().unwrap(), manifest);
    assert_eq!(ygg::knowledge::export::publish(&single, &empty, &archive, &published).await.unwrap(), manifest);
    assert!(ygg::knowledge::export::publish(&single, &empty, &archive, &archive.join("corpus")).await.is_err());
    assert!(ygg::knowledge::export::publish(&single, &empty, &archive, &empty.join("nested")).await.is_err());
    assert!(!empty.join("nested").exists());
    ygg::knowledge::store::KnowledgeBackup::verify(&archive).unwrap();
    let published_note = published.join(manifest.entries.iter().find(|e| e.key.id == note).unwrap().key.relative_path());
    let original = std::fs::read(&published_note).unwrap();
    std::fs::write(&published_note, b"independent target edit").unwrap();
    assert!(ygg::knowledge::export::publish(&single, &empty, &archive, &published).await.is_err());
    assert_eq!(std::fs::read(&published_note).unwrap(), b"independent target edit");
    std::fs::write(&published_note, original).unwrap();
    // The complete immutable archive permits resuming after capture but before
    // publication without duplicating or changing any document.
    let resumed = temp.path().join("resumed-publication");
    assert_eq!(ygg::knowledge::export::publish(&single, &empty, &archive, &resumed).await.unwrap(), manifest);
    let partial_archive = temp.path().join("partial-archive");
    std::fs::create_dir(&partial_archive).unwrap();
    assert!(ygg::knowledge::export::publish(&single, &empty, &partial_archive, &temp.path().join("never-published")).await.is_err());
    assert!(!temp.path().join("never-published").exists());
    single.close().await;
    let entry = manifest.entries.iter().find(|entry| entry.key.id == note).unwrap();
    let note_path = staging.join(entry.key.relative_path());
    // Recreate a durable partial state: intent + subset of documents, no
    // completion receipt. Resumption must fill only the missing document.
    std::fs::remove_file(staging.join(".export-complete.json")).unwrap();
    std::fs::remove_file(&note_path).unwrap();
    assert!(ygg::knowledge::export::verify(&staging).is_err());
    assert_eq!(ygg::knowledge::export::stage(&f.pool, &mappings, &staging).await.unwrap(), manifest);
    let edited = format!("{}independent edit", std::fs::read_to_string(&note_path).unwrap());
    std::fs::write(&note_path, &edited).unwrap();
    assert!(ygg::knowledge::export::verify(&staging).is_err());
    assert!(ygg::knowledge::export::stage(&f.pool, &mappings, &staging).await.is_err());
    assert_eq!(std::fs::read_to_string(&note_path).unwrap(), edited);
    let marker: (String,i64) = sqlx::query_as("SELECT backend,generation FROM knowledge_storage").fetch_one(&f.pool).await.unwrap();
    assert_eq!(marker, ("fenced".into(),2));
    let saved_plan = std::fs::read(empty.join(".export-plan.json")).unwrap();
    sqlx::query("UPDATE knowledge_storage SET generation=generation+1").execute(&f.pool).await.unwrap();
    assert!(ygg::knowledge::export::stage(&f.pool, &mappings, &empty).await.is_err());
    assert_eq!(std::fs::read(empty.join(".export-plan.json")).unwrap(), saved_plan);
    assert_eq!(ygg::knowledge::export::verify(&empty).unwrap(), empty_manifest);
    let stale_target = temp.path().join("stale-publication");
    assert!(ygg::knowledge::export::publish(&f.pool, &empty, &archive, &stale_target).await.is_err());
    assert!(!stale_target.exists());

    }).catch_unwind().await;
    f.cleanup().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn command_requires_explicit_dry_run() {
    let result = Command::new(env!("CARGO_BIN_EXE_ygg"))
        .args(["knowledge", "migrate"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--dry-run"));
}

#[test]
fn mapping_files_reject_duplicate_owner_definitions() {
    let text = format!(
        r#"{{"database_id":"{}","corpus_id":"{}","repos":{{}},"users":{{"":"alice","":"bob"}}}}"#,
        Uuid::new_v4(),
        Uuid::new_v4()
    );
    assert!(serde_json::from_str::<Mappings>(&text).is_err());
}
