//! Session-lock authority for daemons. Losing the dedicated backend is fatal;
//! pooled connections must not let a former owner silently keep working.
use anyhow::{Context, Result, ensure};
use sqlx::{PgConnection, PgPool};
use std::{future::Future, time::Duration};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

pub struct SingletonGuard {
    connection: PgConnection,
    key: i64,
    backend: i32,
    valid: bool,
}

impl SingletonGuard {
    pub async fn try_acquire(pool: &PgPool, key: i64) -> Result<Option<Self>> {
        let mut connection = pool.acquire().await?.detach();
        let (acquired, backend): (bool, i32) = tokio::time::timeout(
            PROBE_TIMEOUT,
            sqlx::query_as("SELECT pg_try_advisory_lock($1), pg_backend_pid()")
                .bind(key)
                .fetch_one(&mut connection),
        )
        .await
        .context("singleton acquisition timed out")??;
        Ok(acquired.then_some(Self {
            connection,
            key,
            backend,
            valid: true,
        }))
    }

    pub fn backend_pid(&self) -> i32 {
        self.backend
    }

    /// Verify the original session still owns the original lock. Checking only
    /// SELECT 1 would miss explicit unlocks and session-switching poolers.
    pub async fn verify(&mut self) -> Result<()> {
        ensure!(
            self.valid,
            "singleton authority already lost; acquire a new guard"
        );
        let bits = self.key as u64;
        let result = tokio::time::timeout(PROBE_TIMEOUT, sqlx::query_scalar::<_, bool>(
            "SELECT pg_backend_pid() = $1 AND EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND classid::bigint = $2 AND objid::bigint = $3 AND objsubid = 1 AND mode = 'ExclusiveLock' AND granted)")
            .bind(self.backend).bind((bits >> 32) as i64).bind((bits & 0xffff_ffff) as i64)
            .fetch_one(&mut self.connection)).await;
        match result {
            Ok(Ok(true)) => Ok(()),
            _ => {
                self.valid = false;
                anyhow::bail!(
                    "singleton authority lost: original backend or advisory lock unavailable; stop work and acquire a new session lock (direct or session-preserving connection required)"
                )
            }
        }
    }

    /// Check before polling work, then monitor while it is running. On loss,
    /// drop the work future and return an error; never replay an in-flight
    /// mutation whose commit status may be unknown. Already-issued SQL or OS
    /// side effects cannot be recalled by cancelling a Rust future.
    pub async fn supervise<T>(&mut self, work: impl Future<Output = Result<T>>) -> Result<T> {
        self.verify().await?;
        let monitor = async {
            loop {
                tokio::time::sleep(PROBE_INTERVAL).await;
                self.verify().await?;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        tokio::pin!(monitor);
        tokio::pin!(work);
        tokio::select! {
            biased;
            lost = &mut monitor => {
                lost?;
                anyhow::bail!("singleton monitor stopped")
            }
            result = &mut work => result,
        }
    }
}
