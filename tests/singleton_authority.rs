use futures::FutureExt;
use sqlx::{PgPool, postgres::PgConnectOptions};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::{sleep, timeout};
use ygg::db::singleton::SingletonGuard;

async fn pool() -> PgPool {
    ygg::db::create_pool(&std::env::var("DATABASE_URL").expect("DATABASE_URL required"))
        .await
        .unwrap()
}

#[tokio::test]
async fn terminated_session_cancels_work_and_requires_new_authority() {
    let pool = pool().await;
    let key = (uuid::Uuid::new_v4().as_u128() as i64) & i64::MAX;
    let mut guard = SingletonGuard::try_acquire(&pool, key)
        .await
        .unwrap()
        .unwrap();
    assert!(
        SingletonGuard::try_acquire(&pool, key)
            .await
            .unwrap()
            .is_none()
    );
    let pid = guard.backend_pid();
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let work_started = started.clone();
    let work_dropped = dropped.clone();
    let worker = tokio::spawn(async move {
        let result = guard
            .supervise(async {
                let _flag = DropFlag(work_dropped);
                work_started.notify_one();
                std::future::pending::<()>().await;
                Ok(())
            })
            .await;
        (guard, result)
    });
    timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(killed);
    let (mut guard, result) = timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("authority lost"));
    assert!(dropped.load(Ordering::SeqCst));
    // The ordinary pool still works: it must not revive the old guard.
    sqlx::query("SELECT 1").execute(&pool).await.unwrap();
    let polled = AtomicBool::new(false);
    assert!(
        guard
            .supervise(async {
                polled.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .is_err()
    );
    assert!(!polled.load(Ordering::SeqCst));
    let mut replacement = SingletonGuard::try_acquire(&pool, key)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.backend_pid(), pid);
    replacement.verify().await.unwrap();
}

#[tokio::test]
async fn scheduler_and_watcher_exit_when_their_lock_backend_dies() {
    let admin = pool().await;
    let name = format!("ygg_authority_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let options: PgConnectOptions = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options.database(&name))
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        ygg::db::run_migrations(&pool).await.unwrap();
        let app = ygg::config::AppConfig::from_env().unwrap();
        for (key, scheduler) in [(0x4347_4753_4348_i64, true), (0x4347_5743_4800_i64, false)] {
            let daemon_pool = pool.clone();
            let mut app = app.clone();
            app.watcher_interval_secs = 60;
            let daemon = tokio::spawn(async move {
                if scheduler {
                    let mut cfg = ygg::scheduler::SchedulerConfig::from_app(&app);
                    cfg.tick_interval = Duration::from_secs(60);
                    ygg::scheduler::run(daemon_pool, cfg).await
                } else {
                    ygg::watcher::Watcher::new(daemon_pool, app).run().await
                }
            });
            let pid: i32 = timeout(Duration::from_secs(5), async {
                loop {
                    let pid = sqlx::query_scalar::<_, i32>("SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND classid::bigint = $1 AND objid::bigint = $2 AND objsubid = 1 AND granted")
                        .bind(key >> 32).bind(key & 0xffff_ffff).fetch_optional(&pool).await.unwrap();
                    if let Some(pid) = pid { break pid; }
                    sleep(Duration::from_millis(20)).await;
                }
            }).await.unwrap();
            assert!(sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)").bind(pid).fetch_one(&pool).await.unwrap());
            let error = timeout(Duration::from_secs(5), daemon).await.unwrap().unwrap().unwrap_err();
            assert!(error.to_string().contains("authority lost"), "{error}");
            let mut replacement = SingletonGuard::try_acquire(&pool, key).await.unwrap().unwrap();
            replacement.verify().await.unwrap();
        }
    }).catch_unwind().await;
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
