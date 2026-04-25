//! `ygg forget` — retroactively scrub content from already-stored data.
//! Two modes:
//!
//!   ygg forget --node <uuid>          delete a specific node + its embedding cache entry
//!   ygg forget --pattern <substring>  redact any node whose JSON content contains the substring
//!   ygg forget --redact-all            run the redactor over every existing node's content
//!
//! This is the "oh god I pasted a secret" escape hatch. Writes a
//! `redaction_applied` event for each affected row so the cleanup is auditable.

use uuid::Uuid;

use crate::models::event::{EventKind, EventRepo};
use crate::redaction;

pub async fn forget_node(pool: &sqlx::PgPool, node_id: Uuid) -> Result<(), anyhow::Error> {
    let res = sqlx::query("DELETE FROM nodes WHERE id = $1")
        .bind(node_id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("node {node_id} not found");
    }
    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RedactionApplied,
            "ygg-forget",
            None,
            serde_json::json!({
                "action": "node_deleted",
                "node_id": node_id,
            }),
        )
        .await;
    println!("Deleted node {node_id}.");
    Ok(())
}

pub async fn forget_pattern(pool: &sqlx::PgPool, substring: &str) -> Result<(), anyhow::Error> {
    // Pull candidates with the substring anywhere in their content,
    // redact each, update.
    let rows: Vec<(Uuid, serde_json::Value)> =
        sqlx::query_as("SELECT id, content FROM nodes WHERE content::text ILIKE $1")
            .bind(format!("%{}%", substring))
            .fetch_all(pool)
            .await?;

    if rows.is_empty() {
        println!("No nodes matched.");
        return Ok(());
    }

    let mut updated = 0;
    for (id, content) in &rows {
        // Replace the substring with [redacted:manual] in every string field.
        let scrubbed = replace_substring_in_json(content.clone(), substring, "[redacted:manual]");
        sqlx::query("UPDATE nodes SET content = $2 WHERE id = $1")
            .bind(id)
            .bind(&scrubbed)
            .execute(pool)
            .await?;
        updated += 1;
    }

    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RedactionApplied,
            "ygg-forget",
            None,
            serde_json::json!({
                "action": "pattern_redaction",
                "pattern": substring,
                "nodes_affected": updated,
            }),
        )
        .await;

    println!("Redacted substring in {updated} node(s).");
    Ok(())
}

pub async fn redact_all(pool: &sqlx::PgPool) -> Result<(), anyhow::Error> {
    let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as("SELECT id, content FROM nodes")
        .fetch_all(pool)
        .await?;

    let mut total_secrets = 0u32;
    let mut nodes_affected = 0u32;
    for (id, content) in rows {
        let (scrubbed, report) = redaction::redact_json(content);
        if report.is_clean() {
            continue;
        }
        sqlx::query("UPDATE nodes SET content = $2 WHERE id = $1")
            .bind(id)
            .bind(&scrubbed)
            .execute(pool)
            .await?;
        nodes_affected += 1;
        total_secrets += report.total;
    }

    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RedactionApplied,
            "ygg-forget",
            None,
            serde_json::json!({
                "action": "bulk_redaction",
                "nodes_affected": nodes_affected,
                "total_secrets": total_secrets,
            }),
        )
        .await;

    println!("Scanned all nodes · redacted {total_secrets} secret(s) in {nodes_affected} node(s).");
    Ok(())
}

fn replace_substring_in_json(mut v: serde_json::Value, pat: &str, repl: &str) -> serde_json::Value {
    fn walk(v: &mut serde_json::Value, pat: &str, repl: &str) {
        match v {
            serde_json::Value::String(s) => {
                if s.contains(pat) {
                    *s = s.replace(pat, repl);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    walk(item, pat, repl);
                }
            }
            serde_json::Value::Object(obj) => {
                for (_, val) in obj {
                    walk(val, pat, repl);
                }
            }
            _ => {}
        }
    }
    walk(&mut v, pat, repl);
    v
}
