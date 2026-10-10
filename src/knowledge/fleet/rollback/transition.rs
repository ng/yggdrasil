use super::*;
use sqlx::{Postgres, Transaction};

#[derive(Clone)]
pub(in crate::knowledge::fleet) struct ReverseFence {
    pub generation: i64,
    pub hosts_sha256: String,
    pub hosts_json: String,
    pub remote_commit: String,
}
impl RollbackPlan {
    async fn saved_fence_on(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<ReverseFence>> {
        let row: Option<(i64,String,String,String)> = sqlx::query_as("SELECT generation,hosts_sha256,hosts_json,remote_commit FROM public.knowledge_fleet_rollback_fences WHERE operation_id=$1")
            .bind(self.operation()).fetch_optional(&mut **tx).await?;
        Ok(row.map(
            |(generation, hosts_sha256, hosts_json, remote_commit)| ReverseFence {
                generation,
                hosts_sha256,
                hosts_json,
                remote_commit,
            },
        ))
    }
    pub(super) async fn host_phase_on(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<ReverseFence>> {
        let marker: (Uuid,i64,i32,String,Option<Uuid>) = sqlx::query_as("SELECT database_id,generation,minimum_client,backend,corpus_id FROM public.knowledge_storage WHERE singleton")
            .fetch_one(&mut **tx).await?;
        ensure!(
            marker.0 == self.forward.database_id
                && marker.4 == Some(self.forward.corpus_id)
                && marker.2 > 0
                && marker.2 <= crate::knowledge::guard::CLIENT_PROTOCOL,
            "rollback database, corpus or protocol differs"
        );
        let saved = self.saved_fence_on(tx).await?;
        if marker.1 == self.request.source_generation && marker.3 == "okf" {
            ensure!(saved.is_none(), "recorded rollback fence no longer current");
            return Ok(None);
        }
        ensure!(
            marker.1 == self.request.source_generation + 1 && marker.3 == "fenced",
            "rollback requires original active or owned reverse-fenced generation"
        );
        let saved = saved.context("matching committed rollback fence missing")?;
        ensure!(
            saved.generation == marker.1
                && saved.remote_commit == self.request.expected_remote_commit
                && digest(saved.hosts_json.as_bytes()) == saved.hosts_sha256
                && serde_json::from_str::<serde_json::Value>(&saved.hosts_json)?["rollback_sha256"]
                    == self.sha256,
            "committed rollback fence differs"
        );
        Ok(Some(saved))
    }
    pub(in crate::knowledge::fleet) async fn current_fence(
        &self,
        pool: &sqlx::PgPool,
    ) -> Result<Option<ReverseFence>> {
        let mut tx = Registration::transaction(pool).await?;
        let saved = self.host_phase_on(&mut tx).await?;
        if saved.is_some() {
            self.registered_on(&mut tx).await?;
        } else {
            let active = forward_transition::saved_activation(&self.forward, &mut tx)
                .await?
                .context("fleet activation missing")?;
            forward_transition::verify_activation(&self.forward, &mut tx, &active).await?;
            ensure!(
                active.ready_sha256 == self.request.activation_sha256,
                "rollback activation differs"
            );
        }
        tx.commit().await?;
        Ok(saved)
    }
    pub(in crate::knowledge::fleet) async fn commit_fence(
        &self,
        pool: &sqlx::PgPool,
        hosts_json: &str,
        verify: impl Fn() -> Result<()>,
    ) -> Result<ReverseFence> {
        ensure!(
            hosts_json.len() <= 64 * 1024 * 1024,
            "rollback host evidence exceeds 64 MiB"
        );
        let mut tx = Registration::transaction(pool).await?;
        self.registered_on(&mut tx).await?;
        let saved = self.host_phase_on(&mut tx).await?;
        verify()?;
        if let Some(saved) = saved {
            ensure!(
                saved.hosts_json == hosts_json,
                "fresh host fences differ from committed census"
            );
            tx.commit().await?;
            return Ok(saved);
        }
        ensure!(
            crate::knowledge::clients::audit(&mut tx)
                .await?
                .live_blockers
                == 0,
            "incompatible live clients block rollback fence"
        );
        let result = ReverseFence {
            generation: self.request.source_generation + 1,
            hosts_sha256: digest(hosts_json.as_bytes()),
            hosts_json: hosts_json.to_owned(),
            remote_commit: self.request.expected_remote_commit.clone(),
        };
        sqlx::query(
            "UPDATE public.knowledge_storage SET generation=$1,backend='fenced' WHERE singleton",
        )
        .bind(result.generation)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO public.knowledge_fleet_rollback_fences(operation_id,generation,hosts_sha256,hosts_json,remote_commit) VALUES($1,$2,$3,$4,$5)")
            .bind(self.operation()).bind(result.generation).bind(&result.hosts_sha256).bind(&result.hosts_json).bind(&result.remote_commit)
            .execute(&mut *tx).await?;
        verify()?;
        tx.commit()
            .await
            .context("reverse fence outcome uncertain; resume exact rollback request")?;
        Ok(result)
    }
}
