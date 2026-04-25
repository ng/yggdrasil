//! Epoch reflections — yggdrasil-10.
//!
//! Fires a background digest when the current transcript's token estimate
//! has grown by more than `YGG_EPOCH_DELTA_TOKENS` since the last digest
//! (or session start). Triggered from the tail of `ygg inject` so it
//! runs once per user turn with zero extra infrastructure; the digest
//! itself runs on `tokio::spawn` so it doesn't block the inject pipeline.
//!
//! This is how we guard against Claude-Code's auto-compaction: by the
//! time CC compacts, Yggdrasil has already written a structured digest
//! that `ygg prime` can surface on the next session / after compaction.
//!
//! Mark tracking lives in `agents.metadata` JSONB so we don't need a new
//! column. Two keys:
//!   last_epoch_tokens: transcript token-estimate at last digest
//!   last_epoch_at:     ISO8601 timestamp of that digest

use tracing::{debug, info, warn};
use uuid::Uuid;

const DEFAULT_DELTA_TOKENS: i64 = 20_000;

/// Check whether an epoch digest should fire, and if so, spawn one in
/// the background. Non-blocking. Safe to call on every inject.
pub async fn maybe_fire(pool: &sqlx::PgPool, agent_id: Uuid, agent_name: &str) {
    if std::env::var("YGG_EPOCH").ok().as_deref() == Some("off") {
        return;
    }

    let delta: i64 = std::env::var("YGG_EPOCH_DELTA_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DELTA_TOKENS);

    let Some(transcript) = crate::cli::digest::find_latest_transcript() else {
        return;
    };
    let Some(current_tokens) = transcript_token_estimate(&transcript) else {
        return;
    };

    // Pull last-epoch mark from agents.metadata. Missing = 0.
    let last_tokens: i64 = sqlx::query_scalar(
        "SELECT COALESCE((metadata->>'last_epoch_tokens')::bigint, 0)
         FROM agents WHERE agent_id = $1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    if current_tokens.saturating_sub(last_tokens) < delta {
        debug!(
            "epoch: {current_tokens} tok, delta {} < {delta}, skipping",
            current_tokens.saturating_sub(last_tokens)
        );
        return;
    }

    info!(
        "epoch: firing digest — transcript ~{current_tokens} tok (+{} since last)",
        current_tokens - last_tokens
    );

    // Update the mark BEFORE spawning so two concurrent injects don't both
    // fire. Uses JSONB merge so we don't clobber other metadata keys.
    let now = chrono::Utc::now().to_rfc3339();
    let update_res = sqlx::query(
        r#"UPDATE agents
           SET metadata = metadata
             || jsonb_build_object(
                'last_epoch_tokens', $2::bigint,
                'last_epoch_at', $3::text)
           WHERE agent_id = $1
             AND COALESCE((metadata->>'last_epoch_tokens')::bigint, 0) < $2::bigint"#,
    )
    .bind(agent_id)
    .bind(current_tokens)
    .bind(&now)
    .execute(pool)
    .await;

    match update_res {
        Ok(r) if r.rows_affected() == 0 => {
            // Another concurrent caller beat us to the mark update.
            debug!("epoch: raced with another inject, skipping spawn");
            return;
        }
        Err(e) => {
            warn!("epoch: mark-update failed ({e}), skipping");
            return;
        }
        _ => {}
    }

    // Spawn the digest in the background. Clones everything it needs so
    // the caller can return.
    let pool2 = pool.clone();
    let agent_name_owned = agent_name.to_string();
    let transcript_owned = transcript.clone();
    tokio::spawn(async move {
        let Ok(cfg) = crate::config::AppConfig::from_env() else {
            return;
        };
        match crate::cli::digest::execute(&pool2, &cfg, &agent_name_owned, &transcript_owned).await
        {
            Ok(()) => info!("epoch: background digest complete"),
            Err(e) => warn!("epoch: background digest failed: {e}"),
        }
    });
}

/// Rough token estimate from transcript bytes. Claude Code JSONL has
/// heavy framing; 1 token ≈ 10 bytes is the standard approximation
/// (matches what ygg prime uses for its pressure warning).
fn transcript_token_estimate(path: &str) -> Option<i64> {
    let bytes = std::fs::metadata(path).ok()?.len() as i64;
    Some(bytes / 10)
}
