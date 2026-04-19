//! `ygg eval` — aggregate events into a one-page effectiveness report.
//! See docs/llm-usage.md and ADR 0012 for the evaluation rationale.

use chrono::{DateTime, Duration, Utc};

// ANSI
const RESET: &str = "\x1b[0m";
const BOLD:  &str = "\x1b[1m";
const DIM:   &str = "\x1b[2m";
const GREEN: &str = "\x1b[38;5;114m";
const CYAN:  &str = "\x1b[38;5;81m";
const YELL:  &str = "\x1b[38;5;221m";
const ORANG: &str = "\x1b[38;5;208m";
const MAG:   &str = "\x1b[38;5;171m";

pub async fn execute(pool: &sqlx::PgPool, window_hours: i64) -> Result<(), anyhow::Error> {
    let since: DateTime<Utc> = Utc::now() - Duration::hours(window_hours);
    let window = format!("last {window_hours}h");

    println!();
    println!("  {BOLD}ygg eval{RESET}  {DIM}· {window}{RESET}");
    println!("  {DIM}──────────────────────────────────────────────────────────{RESET}");

    // ── Retrieval ────────────────────────────────────────────────────────
    let (hits, avg_per_turn, score_sum, score_n, prompts) = retrieval_stats(pool, since).await?;
    println!();
    println!("  {CYAN}Retrieval{RESET}");
    println!("    user prompts processed .......... {prompts}");
    println!("    similarity hits emitted ......... {hits}");
    println!("    avg hits per prompt ............. {avg_per_turn:.1}");
    if score_n > 0 {
        println!("    avg score of kept hits .......... {:.2}", score_sum / score_n as f64);
    }

    // Reference rate — the real "did it help?" number
    let referenced = count_events(pool, since, "hit_referenced").await?;
    if hits > 0 && referenced > 0 {
        let rate = referenced as f64 / hits as f64 * 100.0;
        println!("    referenced by next turn ......... {GREEN}{referenced}/{hits} ({rate:.0}%){RESET}");
    } else if hits > 0 {
        println!("    referenced by next turn ......... {DIM}0/{hits} (0%) — digest hasn't scored yet{RESET}");
    }

    let (kept, dropped) = scoring_stats(pool, since).await?;
    println!("    scoring: kept / dropped ......... {kept} / {dropped}");
    if let Some(breakdown) = drop_reason_breakdown(pool, since).await? {
        println!("      drop reasons .................. {breakdown}");
    }

    let (cls_kept, cls_dropped, cls_bypassed) = classifier_stats(pool, since).await?;
    if cls_kept + cls_dropped + cls_bypassed > 0 {
        println!("    classifier: kept/drop/bypass .... {cls_kept} / {cls_dropped} / {cls_bypassed}");
    } else {
        println!("    classifier ...................... {DIM}disabled{RESET}");
    }

    let (emb_calls, cache_hits) = embedding_stats(pool, since).await?;
    let total = emb_calls + cache_hits;
    if total > 0 {
        let rate = cache_hits as f64 / total as f64 * 100.0;
        let saved = cache_hits;
        println!(
            "    embedding cache hit rate ........ {GREEN}{rate:.0}%{RESET} ({cache_hits} / {total}) — {saved} Ollama calls saved"
        );
    }

    // ── Context ──────────────────────────────────────────────────────────
    let digests = count_events(pool, since, "digest_written").await?;
    let node_writes = count_events(pool, since, "node_written").await?;
    println!();
    println!("  {YELL}Context{RESET}");
    println!("    nodes written ................... {node_writes}");
    println!("    digests written ................. {digests}");

    // ── Coordination ─────────────────────────────────────────────────────
    let locks = count_events(pool, since, "lock_acquired").await?;
    let releases = count_events(pool, since, "lock_released").await?;
    println!();
    println!("  {ORANG}Coordination{RESET}");
    println!("    locks acquired / released ....... {locks} / {releases}");

    // ── Tasks ────────────────────────────────────────────────────────────
    let task_created = count_events(pool, since, "task_created").await?;
    let task_status = count_events(pool, since, "task_status_changed").await?;
    let remembered = count_events(pool, since, "remembered").await?;
    println!();
    println!("  {GREEN}Work{RESET}");
    println!("    tasks created ................... {task_created}");
    println!("    task status changes ............. {task_status}");
    println!("    remembered directives ........... {remembered}");

    // ── Learnings ────────────────────────────────────────────────────────
    // Scoped CodeRabbit-style rules. Fired == applied_count incremented
    // when the hook surfaces a learning whose scope matches the task's
    // file set (yggdrasil-82, not yet shipped). `fired` will read 0 until
    // then; `new` and `total` land as soon as `ygg learn create` runs.
    let (learnings_total, learnings_new, learnings_fired_total): (i64, i64, i64) = sqlx::query_as(
        r#"SELECT
             COUNT(*)::bigint,
             COUNT(*) FILTER (WHERE created_at >= $1)::bigint,
             COALESCE(SUM(applied_count), 0)::bigint
           FROM learnings"#,
    )
    .bind(since)
    .fetch_one(pool).await.unwrap_or((0, 0, 0));
    println!();
    println!("  {MAG}Learnings{RESET}");
    println!("    captured in window .............. {learnings_new}");
    println!("    total in db ..................... {learnings_total}");
    if learnings_fired_total > 0 {
        println!("    cumulative applications ......... {GREEN}{learnings_fired_total}{RESET}");
    } else if learnings_total > 0 {
        println!("    cumulative applications ......... {DIM}0 (hook integration pending — yggdrasil-82){RESET}");
    }

    // ── Hit quality ──────────────────────────────────────────────────────
    // Ranks source_node_ids by "surfaced vs referenced". Shows the worst
    // offenders (repeatedly emitted but never used — candidates for
    // forget/demote) and best performers (high reference rate). Point is
    // actionable data, not another aggregate.
    println!();
    println!("  {CYAN}Hit quality{RESET}  {DIM}(per source_node_id in window){RESET}");

    let quality = hit_quality(pool, since, 5).await?;
    if quality.is_empty() {
        println!("    {DIM}(no similarity_hit events yet in this window){RESET}");
    } else {
        println!("    {DIM}worst (surfaced ≥2, never referenced):{RESET}");
        let worst: Vec<_> = quality.iter().filter(|q| q.referenced == 0 && q.emitted >= 2).take(5).collect();
        if worst.is_empty() {
            println!("      {DIM}(none — every frequently-surfaced hit got referenced at least once){RESET}");
        } else {
            for q in worst {
                println!(
                    "      {}× surfaced / 0 ref   {DIM}from {}{RESET}  \"{}\"",
                    q.emitted, q.source_agent, truncate(&q.snippet, 60)
                );
            }
        }
        println!();
        println!("    {DIM}best (highest referenced rate, minimum 2 surfaces):{RESET}");
        let best: Vec<_> = quality.iter()
            .filter(|q| q.emitted >= 2 && q.referenced > 0)
            .take(5)
            .collect();
        if best.is_empty() {
            println!("      {DIM}(none — no hit has been referenced yet, or digest hasn't run){RESET}");
        } else {
            for q in best {
                let rate = (q.referenced as f64 / q.emitted as f64 * 100.0) as i64;
                println!(
                    "      {}/{} ({}%)   {DIM}from {}{RESET}  \"{}\"",
                    q.referenced, q.emitted, rate, q.source_agent, truncate(&q.snippet, 60)
                );
            }
        }
    }

    println!();
    Ok(())
}

#[derive(Debug)]
struct HitQualityRow {
    source_agent: String,
    emitted: i64,
    referenced: i64,
    snippet: String,
}

/// Aggregate similarity_hit / hit_referenced pairs by source_node_id. The
/// referenced side fires at digest time and may land outside `since`; we
/// count it regardless so old emits still credit their late references.
async fn hit_quality(
    pool: &sqlx::PgPool,
    since: DateTime<Utc>,
    _limit: i64,
) -> Result<Vec<HitQualityRow>, anyhow::Error> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        r#"
        WITH emits AS (
            SELECT DISTINCT ON ((payload->>'source_node_id'))
                   (payload->>'source_node_id') AS sid,
                   COALESCE(payload->>'source_agent', '?') AS src_agent,
                   COALESCE(payload->>'snippet', '') AS snip,
                   created_at
              FROM events
             WHERE event_kind::text = 'similarity_hit'
               AND created_at >= $1
               AND payload->>'source_node_id' IS NOT NULL
             ORDER BY (payload->>'source_node_id'), created_at DESC
        ),
        emit_counts AS (
            SELECT (payload->>'source_node_id') AS sid, COUNT(*) AS n_emit
              FROM events
             WHERE event_kind::text = 'similarity_hit'
               AND created_at >= $1
               AND payload->>'source_node_id' IS NOT NULL
             GROUP BY sid
        ),
        refs AS (
            SELECT (payload->>'source_node_id') AS sid, COUNT(*) AS n_ref
              FROM events
             WHERE event_kind::text = 'hit_referenced'
               AND payload->>'source_node_id' IS NOT NULL
             GROUP BY sid
        )
        SELECT e.src_agent, e.snip, ec.n_emit, COALESCE(r.n_ref, 0)
          FROM emits e
          JOIN emit_counts ec ON ec.sid = e.sid
          LEFT JOIN refs r ON r.sid = e.sid
         ORDER BY ec.n_emit DESC, r.n_ref DESC NULLS LAST
         LIMIT 200
        "#,
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(src, snip, emit, refd)| HitQualityRow {
        source_agent: src,
        emitted: emit,
        referenced: refd,
        snippet: snip,
    }).collect())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    s.chars().take(max).collect::<String>() + "…"
}

async fn count_events(pool: &sqlx::PgPool, since: DateTime<Utc>, kind: &str) -> Result<i64, anyhow::Error> {
    let n: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = $1 AND created_at >= $2"
    )
    .bind(kind)
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(n.0)
}

/// Returns (hits_emitted, avg_per_prompt, sum_score, n_with_score, distinct_prompt_nodes)
async fn retrieval_stats(
    pool: &sqlx::PgPool,
    since: DateTime<Utc>,
) -> Result<(i64, f64, f64, i64, i64), anyhow::Error> {
    let hits: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'similarity_hit' AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;

    let (score_sum, score_n): (Option<f64>, i64) = sqlx::query_as(
        "SELECT SUM((payload->>'similarity')::float), COUNT(*)
         FROM events WHERE event_kind::text = 'similarity_hit' AND created_at >= $1
         AND payload->>'similarity' IS NOT NULL"
    ).bind(since).fetch_one(pool).await.unwrap_or((None, 0));

    // Distinct user prompts = user_message node writes in the window.
    let prompts: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events
         WHERE event_kind::text = 'node_written' AND created_at >= $1
         AND payload->>'snippet' IS NOT NULL"
    ).bind(since).fetch_one(pool).await?;

    let avg = if prompts.0 > 0 { hits.0 as f64 / prompts.0 as f64 } else { 0.0 };
    Ok((hits.0, avg, score_sum.unwrap_or(0.0), score_n, prompts.0))
}

async fn scoring_stats(pool: &sqlx::PgPool, since: DateTime<Utc>) -> Result<(i64, i64), anyhow::Error> {
    let kept: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'scoring_decision'
         AND (payload->>'kept')::bool = true AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    let dropped: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'scoring_decision'
         AND (payload->>'kept')::bool = false AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    Ok((kept.0, dropped.0))
}

async fn drop_reason_breakdown(pool: &sqlx::PgPool, since: DateTime<Utc>) -> Result<Option<String>, anyhow::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT payload->>'drop_reason' AS r, COUNT(*) AS n
         FROM events WHERE event_kind::text = 'scoring_decision'
           AND (payload->>'kept')::bool = false AND created_at >= $1
           AND payload->>'drop_reason' IS NOT NULL
         GROUP BY r ORDER BY n DESC"
    ).bind(since).fetch_all(pool).await?;
    if rows.is_empty() { return Ok(None); }
    let s = rows.iter().map(|(r, n)| format!("{r}={n}")).collect::<Vec<_>>().join("  ");
    Ok(Some(s))
}

async fn classifier_stats(pool: &sqlx::PgPool, since: DateTime<Utc>) -> Result<(i64, i64, i64), anyhow::Error> {
    let kept: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'classifier_decision'
         AND (payload->>'kept')::bool = true AND (payload->>'bypassed')::bool = false
         AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    let dropped: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'classifier_decision'
         AND (payload->>'kept')::bool = false AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    let bypassed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'classifier_decision'
         AND (payload->>'bypassed')::bool = true AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    Ok((kept.0, dropped.0, bypassed.0))
}

async fn embedding_stats(pool: &sqlx::PgPool, since: DateTime<Utc>) -> Result<(i64, i64), anyhow::Error> {
    let calls: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'embedding_call' AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    let hits: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events WHERE event_kind::text = 'embedding_cache_hit' AND created_at >= $1"
    ).bind(since).fetch_one(pool).await?;
    Ok((calls.0, hits.0))
}
