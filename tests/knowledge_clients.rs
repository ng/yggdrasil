#![cfg(any(target_os = "macos", target_os = "linux"))]
use futures::FutureExt;
use sqlx::{Connection, PgConnection, postgres::PgPoolOptions};
use uuid::Uuid;
use ygg::knowledge::clients::{self, Compatibility};

#[tokio::test]
async fn client_registry_binds_backend_lifetimes_and_audit_refuses_unknown_clients() {
    let base = std::env::var("DATABASE_URL").expect("disposable DATABASE_URL required");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    let database = format!("ygg_clients_{}", Uuid::new_v4().simple());
    let role = format!("client_runtime_{}", Uuid::new_v4().simple());
    let schema_owner = format!("client_owner_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE ROLE {schema_owner} LOGIN PASSWORD 'fixture-owner-password'"
    ))
    .execute(&admin)
    .await
    .unwrap();
    sqlx::query(&format!("CREATE DATABASE {database} OWNER {schema_owner}"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD 'fixture-only-password'"
    ))
    .execute(&admin)
    .await
    .unwrap();
    let mut url = url::Url::parse(&base).unwrap();
    url.set_path(&format!("/{database}"));
    let result = std::panic::AssertUnwindSafe(async {
        let mut owner = PgConnection::connect(url.as_str()).await.unwrap();
        // The compatibility binary can connect before the new migration exists.
        let old = ygg::db::create_pool(url.as_str()).await.unwrap();
        let mut migration_url = url.clone();
        migration_url.set_username(&schema_owner).unwrap();
        migration_url
            .set_password(Some("fixture-owner-password"))
            .unwrap();
        let mut migrator = PgConnection::connect(migration_url.as_str()).await.unwrap();
        sqlx::migrate!("./migrations")
            .run(&mut migrator)
            .await
            .unwrap();
        migrator.close().await.unwrap();
        let mut stale = owner.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *stale)
            .await
            .unwrap();
        assert!(
            clients::audit(&mut stale)
                .await
                .unwrap_err()
                .to_string()
                .contains("READ COMMITTED")
        );
        stale.rollback().await.unwrap();
        let observed = clients::audit(&mut owner).await.unwrap();
        assert_eq!(observed.clients.len(), 1);
        assert_eq!(observed.live_blockers, 1);
        assert_eq!(
            observed.clients[0].compatibility,
            Compatibility::Unregistered
        );
        old.close().await;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO {role}"))
            .execute(&mut owner)
            .await
            .unwrap();
        let mut runtime_url = url.clone();
        runtime_url.set_username(&role).unwrap();
        runtime_url
            .set_password(Some("fixture-only-password"))
            .unwrap();
        let pool = ygg::db::create_pool(runtime_url.as_str()).await.unwrap();
        let first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        let observed = clients::audit(&mut owner).await.unwrap();
        assert_eq!(observed.clients.len(), 2);
        assert_eq!(observed.live_blockers, 0);
        assert_eq!(
            observed.clients[0].process_id,
            observed.clients[1].process_id
        );
        assert!(observed.clients.iter().all(|c| c.compatibility
                == Compatibility::CompatibleRegistration
                && c.role == role));
        assert_eq!(observed.remaining_verification.len(), 3);
        // Runtime can only declare its own connection through the function.
        for query in [
            "DELETE FROM public.knowledge_clients",
            "UPDATE public.knowledge_clients SET protocol=999",
            "INSERT INTO public.knowledge_clients SELECT * FROM public.knowledge_clients",
        ] {
            let error = sqlx::query(query).execute(&mut *second).await.unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().code().as_deref(),
                Some("42501")
            );
        }
        // Even SELECT permission cannot compensate for incomplete server stats.
        sqlx::query(&format!(
            "GRANT SELECT ON public.knowledge_clients TO {role}"
        ))
        .execute(&mut owner)
        .await
        .unwrap();
        assert!(
            clients::audit(&mut second)
                .await
                .unwrap_err()
                .to_string()
                .contains("pg_read_all_stats")
        );
        let count_before: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_clients")
            .fetch_one(&mut owner)
            .await
            .unwrap();
        let mut unknown = PgConnection::connect(runtime_url.as_str()).await.unwrap();
        let unknown_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut unknown)
            .await
            .unwrap();
        let observed = clients::audit(&mut owner).await.unwrap();
        assert_eq!(observed.live_blockers, 1);
        assert_eq!(
            observed
                .clients
                .iter()
                .find(|c| c.backend_pid == unknown_pid)
                .unwrap()
                .compatibility,
            Compatibility::Unregistered
        );
        // Supplying somebody else's start timestamp cannot register their PID or
        // make this caller match its own server-observed backend lifetime.
        sqlx::query("SELECT public.ygg_knowledge_register_client(1,'fixture',$1,$2)")
            .bind(Uuid::new_v4())
            .bind(
                observed
                    .clients
                    .iter()
                    .find(|c| c.backend_pid != unknown_pid)
                    .unwrap()
                    .backend_start,
            )
            .execute(&mut unknown)
            .await
            .unwrap();
        let observed = clients::audit(&mut owner).await.unwrap();
        assert_eq!(observed.live_blockers, 1);
        assert_eq!(
            observed
                .clients
                .iter()
                .find(|c| c.backend_pid == unknown_pid)
                .unwrap()
                .compatibility,
            Compatibility::Unregistered
        );
        clients::register(&mut unknown).await.unwrap();
        assert_eq!(clients::audit(&mut owner).await.unwrap().live_blockers, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_clients")
                .fetch_one(&mut owner)
                .await
                .unwrap(),
            count_before + 1
        );
        // The read-only CLI emits observations but exits nonzero because this
        // deliberately unregistered inspector remains connected beside it.
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_ygg"))
            .env_clear()
            .env("HOME", temp.path())
            .env("DATABASE_URL", runtime_url.as_str())
            .env("YGG_DATABASE_OWNER_URL", url.as_str())
            .args(["knowledge", "clients", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["live_blockers"], 1);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-only-password"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_clients")
                .fetch_one(&mut owner)
                .await
                .unwrap(),
            count_before + 1
        );
        // A higher server minimum disqualifies previously compatible clients.
        sqlx::query("UPDATE knowledge_storage SET generation=generation+1,minimum_client=2")
            .execute(&mut owner)
            .await
            .unwrap();
        let observed = clients::audit(&mut owner).await.unwrap();
        assert_eq!(observed.live_blockers, 3);
        assert!(
            observed
                .clients
                .iter()
                .all(|c| c.compatibility == Compatibility::ProtocolTooOld)
        );
        unknown.close().await.unwrap();
        // Registration prunes ended sessions, but never another live client.
        clients::register(&mut second).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM knowledge_clients")
                .fetch_one(&mut owner)
                .await
                .unwrap(),
            count_before
        );
        // Compatibility metadata must not block coordination startup while
        // a knowledge transition owns its exclusive migration lease.
        sqlx::query("SELECT pg_advisory_lock(1497843531,1)")
            .execute(&mut owner)
            .await
            .unwrap();
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            ygg::db::create_pool(runtime_url.as_str()),
        )
        .await;
        sqlx::query("SELECT pg_advisory_unlock(1497843531,1)")
            .execute(&mut owner)
            .await
            .unwrap();
        connected.unwrap().unwrap().close().await;
        drop(first);
        drop(second);
        pool.close().await;
        owner.close().await.unwrap();
    })
    .catch_unwind()
    .await;
    sqlx::query(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {role}"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("DROP ROLE {schema_owner}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
