use super::*;
#[derive(Clone)]
pub(in crate::knowledge::fleet) struct CancellationComplete {
    pub hosts_sha256: String,
    pub hosts_json: String,
}
impl RollbackPlan {
    pub(super) async fn saved_cancellation_on(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<Option<CancellationComplete>> {
        let saved: Option<(String,String)> = sqlx::query_as("SELECT hosts_sha256,hosts_json FROM public.knowledge_fleet_rollback_completions WHERE operation_id=$1")
            .bind(self.operation()).fetch_optional(&mut **tx).await?;
        saved.map(|(hosts_sha256, hosts_json)| {
            ensure!(digest(hosts_json.as_bytes()) == hosts_sha256
                && serde_json::from_str::<serde_json::Value>(&hosts_json)?["rollback_sha256"] == self.sha256,
                "committed cancellation census differs");
            Ok(CancellationComplete { hosts_sha256, hosts_json })
        }).transpose()
    }
    /// Historical status only: no host mutation is authorized by this method.
    pub(in crate::knowledge::fleet) async fn cancellation_complete(
        &self,
        pool: &sqlx::PgPool,
    ) -> Result<Option<CancellationComplete>> {
        let mut tx = Registration::transaction(pool).await?;
        self.registered_identity_on(&mut tx).await?;
        let saved = self.saved_cancellation_on(&mut tx).await?;
        tx.commit().await?;
        Ok(saved)
    }
    pub(in crate::knowledge::fleet) async fn seal_cancellation(
        &self,
        pool: &sqlx::PgPool,
        receipt: &CancellationReceipt,
        hosts: &str,
        verify: impl Fn() -> Result<()>,
    ) -> Result<CancellationComplete> {
        ensure!(
            hosts.len() <= 64 * 1024 * 1024,
            "cancellation census exceeds 64 MiB"
        );
        let mut tx = Registration::transaction(pool).await?;
        self.cancellation_on(&mut tx, receipt).await?;
        verify()?;
        if let Some(saved) = self.saved_cancellation_on(&mut tx).await? {
            ensure!(
                saved.hosts_json == hosts,
                "cancellation completion differs from retained census"
            );
            tx.commit().await?;
            return Ok(saved);
        }
        let result = CancellationComplete {
            hosts_sha256: digest(hosts.as_bytes()),
            hosts_json: hosts.to_owned(),
        };
        sqlx::query("INSERT INTO public.knowledge_fleet_rollback_completions(operation_id,hosts_sha256,hosts_json) VALUES($1,$2,$3)")
            .bind(self.operation()).bind(&result.hosts_sha256).bind(&result.hosts_json).execute(&mut *tx).await?;
        verify()?;
        tx.commit()
            .await
            .context("cancellation completion outcome uncertain; resume exact operation")?;
        Ok(result)
    }
}
