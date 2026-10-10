//! Durable coordinator reservations and cancellation. Registration binds a plan
//! digest and declared participant set; it does not authenticate hosts or prove
//! quiescence/readiness. Shared migration execution remains unavailable.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod journal;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod plan;

use super::guard::CLIENT_PROTOCOL;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::BTreeSet;
use uuid::Uuid;

type Stored = (String, Uuid, i64, Uuid, Vec<Uuid>);
type Marker = (Uuid, i64, i32, String, Option<Uuid>);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub version: u32,
    pub operation: Uuid,
    /// SHA-256 of the complete, validated coordinator plan, not a host receipt.
    pub request_sha256: String,
    pub database_id: Uuid,
    pub source_generation: i64,
    pub corpus_id: Uuid,
    pub participants: Vec<Uuid>,
}
impl Registration {
    fn expected(&self) -> Result<Stored> {
        ensure!(
            self.version == 1
                && !self.operation.is_nil()
                && !self.database_id.is_nil()
                && !self.corpus_id.is_nil()
                && self.source_generation > 0
                && self.source_generation < i64::MAX - 2,
            "invalid fleet registration identity/version/generation"
        );
        ensure!(
            self.request_sha256.len() == 64
                && self
                    .request_sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "fleet registration requires the complete plan SHA-256"
        );
        let participants: BTreeSet<_> = self.participants.iter().copied().collect();
        ensure!(
            !participants.is_empty()
                && participants.len() <= 1024
                && participants.len() == self.participants.len()
                && !participants.contains(&Uuid::nil()),
            "fleet participants must contain 1..=1024 unique non-nil IDs"
        );
        Ok((
            self.request_sha256.clone(),
            self.database_id,
            self.source_generation,
            self.corpus_id,
            participants.into_iter().collect(),
        ))
    }
    async fn transaction(pool: &PgPool) -> Result<Transaction<'static, Postgres>> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        let owner: bool = sqlx::query_scalar("SELECT pg_catalog.pg_has_role(session_user,relowner,'USAGE') AND pg_catalog.pg_has_role(current_user,relowner,'USAGE') FROM pg_catalog.pg_class WHERE oid='public.knowledge_storage'::regclass")
            .fetch_one(&mut *tx).await?;
        ensure!(owner, "fleet coordination requires migration owner");
        sqlx::query("SELECT pg_advisory_xact_lock(1497843531,1)")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
    async fn saved(&self, tx: &mut Transaction<'_, Postgres>) -> Result<Option<Stored>> {
        Ok(sqlx::query_as("SELECT request_sha256,database_id,source_generation,corpus_id,participants FROM public.knowledge_fleet_operations WHERE operation_id=$1")
            .bind(self.operation).fetch_optional(&mut **tx).await?)
    }
    async fn marker(tx: &mut Transaction<'_, Postgres>) -> Result<Marker> {
        Ok(sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton FOR UPDATE")
            .fetch_one(&mut **tx).await?)
    }
    fn verify_sql(&self, marker: &Marker, generation: i64) -> Result<()> {
        ensure!(
            marker.0 == self.database_id
                && marker.1 == generation
                && marker.2 > 0
                && marker.2 <= CLIENT_PROTOCOL
                && marker.3 == "sql"
                && marker.4.is_none(),
            "fleet operation requires the expected compatible SQL generation"
        );
        Ok(())
    }
    /// Reserve this SQL source for one fleet operation. A matching retry is a
    /// no-op; changing the plan, corpus or participant set cannot adopt it.
    /// The caller must validate/authenticate the complete plan before calling.
    pub async fn register(&self, pool: &PgPool) -> Result<()> {
        let expected = self.expected()?;
        let mut tx = Self::transaction(pool).await?;
        self.verify_sql(&Self::marker(&mut tx).await?, self.source_generation)?;
        let cancelled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public.knowledge_migration_cancellations WHERE operation_id=$1)")
            .bind(self.operation).fetch_one(&mut *tx).await?;
        ensure!(
            !cancelled,
            "fleet operation was cancelled; a new operation is required"
        );
        if let Some(saved) = self.saved(&mut tx).await? {
            ensure!(
                saved == expected,
                "fleet registration differs from retained plan or participants"
            );
        } else {
            let owner: Option<Uuid> = sqlx::query_scalar("SELECT operation_id FROM public.knowledge_fleet_operations WHERE database_id=$1 AND source_generation=$2")
                .bind(self.database_id).bind(self.source_generation).fetch_optional(&mut *tx).await?;
            ensure!(
                owner.is_none(),
                "another fleet operation owns this SQL source generation"
            );
            sqlx::query("INSERT INTO public.knowledge_fleet_operations(operation_id,request_sha256,database_id,source_generation,corpus_id,participants) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(self.operation).bind(&expected.0).bind(expected.1).bind(expected.2)
                .bind(expected.3).bind(&expected.4).execute(&mut *tx).await?;
        }
        tx.commit()
            .await
            .context("fleet registration outcome uncertain; retry the same plan")
    }
    /// Cancel only before SQL fencing. Advance SQL generation and record the
    /// immutable cancellation atomically; participant cancellation consumes this
    /// receipt. No host is implicitly restored and no knowledge rows are replayed.
    pub async fn cancel(&self, pool: &PgPool) -> Result<i64> {
        let expected = self.expected()?;
        let mut tx = Self::transaction(pool).await?;
        ensure!(
            self.saved(&mut tx).await? == Some(expected),
            "matching fleet registration required for cancellation"
        );
        let prior: Option<(String, Uuid, i64, i64)> = sqlx::query_as("SELECT request_sha256,database_id,source_generation,target_generation FROM public.knowledge_migration_cancellations WHERE operation_id=$1")
            .bind(self.operation).fetch_optional(&mut *tx).await?;
        let target = self.source_generation + 1;
        if let Some(ref prior) = prior {
            ensure!(
                *prior
                    == (
                        self.request_sha256.clone(),
                        self.database_id,
                        self.source_generation,
                        target
                    ),
                "fleet cancellation receipt differs from registered plan"
            );
        }
        self.verify_sql(
            &Self::marker(&mut tx).await?,
            if prior.is_some() {
                target
            } else {
                self.source_generation
            },
        )?;
        if prior.is_none() {
            sqlx::query("UPDATE public.knowledge_storage SET generation=$1 WHERE singleton")
                .bind(target)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO public.knowledge_migration_cancellations(operation_id,request_sha256,database_id,source_generation,target_generation) VALUES($1,$2,$3,$4,$5)")
                .bind(self.operation).bind(&self.request_sha256).bind(self.database_id)
                .bind(self.source_generation).bind(target).execute(&mut *tx).await?;
        }
        tx.commit()
            .await
            .context("fleet cancellation outcome uncertain; retry the same plan")?;
        Ok(target)
    }
}
