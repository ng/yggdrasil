//! `ygg rollup` — per-repo activity summary over a time window.
//!
//! Aggregates tasks (created/closed), digest summaries, correction themes,
//! and retrieval stats. Output is Markdown so users can pipe it to a file
//! or into a weekly review doc. JSON output is there for tooling; plain-
//! text for quick terminal reads.

use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::models::repo::{Repo, RepoRepo};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

pub async fn execute(
    pool: &PgPool,
    days: i64,
    repo_filter: Option<&str>,
    format: Format,
) -> Result<(), anyhow::Error> {
    let since = Utc::now() - Duration::days(days);
    let repos = RepoRepo::new(pool).list().await.unwrap_or_default();
    let repos: Vec<Repo> = match repo_filter {
        Some(prefix) => repos
            .into_iter()
            .filter(|r| r.task_prefix == prefix)
            .collect(),
        None => repos,
    };

    if repos.is_empty() {
        if let Some(p) = repo_filter {
            anyhow::bail!("no repo with prefix '{p}'");
        }
        println!("(no repos registered)");
        return Ok(());
    }

    // Collect per-repo rollups.
    let mut rollups: Vec<RepoRollup> = Vec::new();
    for repo in &repos {
        rollups.push(build_repo_rollup(pool, repo, since).await?);
    }

    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(
                &rollups.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
            )?;
            println!("{json}");
        }
        Format::Markdown => print_markdown(days, &rollups),
        Format::Text => print_text(days, &rollups),
    }
    Ok(())
}

struct RepoRollup {
    repo: Repo,
    since: DateTime<Utc>,
    tasks_created: Vec<TaskRow>,
    tasks_closed: Vec<TaskRow>,
    digests: Vec<DigestRow>,
    corrections: Vec<String>,
    reinforcements: Vec<String>,
    prompts: i64,
    digests_count: i64,
    similarity_hits: i64,
    cache_hits: i64,
    cache_total: i64,
    redactions: i64,
}

struct TaskRow {
    seq: i32,
    kind: String,
    priority: i16,
    title: String,
}

struct DigestRow {
    created_at: DateTime<Utc>,
    agent_name: String,
    summary: String,
}

async fn build_repo_rollup(
    pool: &PgPool,
    repo: &Repo,
    since: DateTime<Utc>,
) -> Result<RepoRollup, anyhow::Error> {
    // Tasks created in the window, for this repo.
    let created: Vec<(i32, String, i16, String)> = sqlx::query_as(
        r#"SELECT seq, kind::text, priority, title FROM tasks
           WHERE repo_id = $1 AND created_at >= $2
           ORDER BY priority ASC, seq ASC"#,
    )
    .bind(repo.repo_id)
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let tasks_created: Vec<TaskRow> = created
        .into_iter()
        .map(|(seq, kind, priority, title)| TaskRow {
            seq,
            kind,
            priority,
            title,
        })
        .collect();

    // Tasks closed in the window, for this repo.
    let closed: Vec<(i32, String, i16, String)> = sqlx::query_as(
        r#"SELECT seq, kind::text, priority, title FROM tasks
           WHERE repo_id = $1 AND closed_at >= $2 AND status = 'closed'
           ORDER BY closed_at DESC"#,
    )
    .bind(repo.repo_id)
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let tasks_closed: Vec<TaskRow> = closed
        .into_iter()
        .map(|(seq, kind, priority, title)| TaskRow {
            seq,
            kind,
            priority,
            title,
        })
        .collect();

    // Which agents have touched tasks in this repo? We use those agents'
    // activity as the retrieval/context numbers for this repo — events
    // don't carry repo_id directly, so the join is "agents that own any
    // task in this repo" as a proxy.
    let agent_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT assignee FROM tasks
         WHERE repo_id = $1 AND assignee IS NOT NULL",
    )
    .bind(repo.repo_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    // Digests written in window, from those agents, with summary text.
    let digest_rows = if !agent_ids.is_empty() {
        sqlx::query(
            r#"SELECT created_at, agent_name, payload
               FROM events
               WHERE event_kind = 'digest_written'
                 AND created_at >= $1
                 AND agent_id = ANY($2)
               ORDER BY created_at DESC
               LIMIT 20"#,
        )
        .bind(since)
        .bind(&agent_ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let digests: Vec<DigestRow> = digest_rows
        .into_iter()
        .map(|r| {
            let payload: serde_json::Value =
                r.try_get("payload").unwrap_or(serde_json::Value::Null);
            let summary = payload
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            DigestRow {
                created_at: r.get("created_at"),
                agent_name: r.get("agent_name"),
                summary,
            }
        })
        .filter(|d| !d.summary.is_empty())
        .collect();

    // Corrections / reinforcements from those agents.
    let (corrections, reinforcements) = if !agent_ids.is_empty() {
        let corr: Vec<String> = sqlx::query_scalar(
            r#"SELECT payload->>'feedback' FROM events
               WHERE event_kind = 'correction_detected'
                 AND created_at >= $1 AND agent_id = ANY($2)
                 AND COALESCE(payload->>'sentiment', 'correction') = 'correction'
               ORDER BY created_at DESC LIMIT 10"#,
        )
        .bind(since)
        .bind(&agent_ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s: &String| !s.is_empty())
        .collect();
        let rein: Vec<String> = sqlx::query_scalar(
            r#"SELECT payload->>'feedback' FROM events
               WHERE event_kind = 'correction_detected'
                 AND created_at >= $1 AND agent_id = ANY($2)
                 AND payload->>'sentiment' = 'reinforcement'
               ORDER BY created_at DESC LIMIT 10"#,
        )
        .bind(since)
        .bind(&agent_ids)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s: &String| !s.is_empty())
        .collect();
        (corr, rein)
    } else {
        (Vec::new(), Vec::new())
    };

    // Retrieval / activity counters, scoped to those agents.
    let (prompts, digests_count, sim_hits, cache_hits, cache_calls, redactions): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = if !agent_ids.is_empty() {
        sqlx::query_as(
            r#"SELECT
                 COUNT(*) FILTER (WHERE event_kind::text = 'node_written' AND payload->>'kind' = 'user_message'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'digest_written'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'similarity_hit'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_cache_hit'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'embedding_call'),
                 COUNT(*) FILTER (WHERE event_kind::text = 'redaction_applied')
               FROM events
               WHERE created_at >= $1 AND agent_id = ANY($2)"#
        ).bind(since).bind(&agent_ids)
         .fetch_one(pool).await.unwrap_or((0, 0, 0, 0, 0, 0))
    } else {
        (0, 0, 0, 0, 0, 0)
    };

    Ok(RepoRollup {
        repo: repo.clone(),
        since,
        tasks_created,
        tasks_closed,
        digests,
        corrections,
        reinforcements,
        prompts,
        digests_count,
        similarity_hits: sim_hits,
        cache_hits,
        cache_total: cache_hits + cache_calls,
        redactions,
    })
}

fn print_markdown(days: i64, rollups: &[RepoRollup]) {
    let period = match days {
        1 => "day".to_string(),
        7 => "week".to_string(),
        n => format!("{n} days"),
    };
    println!("# Yggdrasil rollup — last {period}");
    println!();
    for r in rollups {
        println!("## {} ({})", r.repo.name, r.repo.task_prefix);
        println!();
        println!("Since: {}", r.since.format("%Y-%m-%d %H:%M UTC"));
        println!();

        if !r.tasks_closed.is_empty() {
            println!("### Closed ({})", r.tasks_closed.len());
            for t in &r.tasks_closed {
                println!(
                    "- `{}-{}` P{} [{}] {}",
                    r.repo.task_prefix, t.seq, t.priority, t.kind, t.title
                );
            }
            println!();
        }

        if !r.tasks_created.is_empty() {
            println!("### Created ({})", r.tasks_created.len());
            for t in &r.tasks_created {
                println!(
                    "- `{}-{}` P{} [{}] {}",
                    r.repo.task_prefix, t.seq, t.priority, t.kind, t.title
                );
            }
            println!();
        }

        if !r.digests.is_empty() {
            println!("### Session digests");
            for d in &r.digests {
                println!(
                    "- {} — {} — {}",
                    d.created_at.format("%Y-%m-%d %H:%M"),
                    d.agent_name,
                    truncate(&d.summary, 140)
                );
            }
            println!();
        }

        if !r.corrections.is_empty() {
            println!("### Corrections");
            for c in &r.corrections {
                println!("- {}", truncate(c, 140));
            }
            println!();
        }

        if !r.reinforcements.is_empty() {
            println!("### Reinforcements");
            for c in &r.reinforcements {
                println!("- {}", truncate(c, 140));
            }
            println!();
        }

        let cache_rate = if r.cache_total > 0 {
            (r.cache_hits as f64 / r.cache_total as f64 * 100.0) as i64
        } else {
            0
        };
        println!("### Stats");
        println!("- prompts: {}", r.prompts);
        println!("- digests: {}", r.digests_count);
        println!("- similarity hits: {}", r.similarity_hits);
        println!(
            "- cache: {}/{} ({}%)",
            r.cache_hits, r.cache_total, cache_rate
        );
        if r.redactions > 0 {
            println!("- redactions: {}", r.redactions);
        }
        println!();
    }
}

fn print_text(days: i64, rollups: &[RepoRollup]) {
    println!("Yggdrasil rollup — last {days} day(s)");
    for r in rollups {
        println!();
        println!("  {} ({})", r.repo.name, r.repo.task_prefix);
        println!(
            "    closed {} · created {} · digests {} · prompts {} · hits {}",
            r.tasks_closed.len(),
            r.tasks_created.len(),
            r.digests_count,
            r.prompts,
            r.similarity_hits
        );
        if !r.tasks_closed.is_empty() {
            println!("    closed:");
            for t in r.tasks_closed.iter().take(5) {
                println!(
                    "      {}-{}  {}",
                    r.repo.task_prefix,
                    t.seq,
                    truncate(&t.title, 60)
                );
            }
        }
    }
}

impl RepoRollup {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "repo": { "name": self.repo.name, "prefix": self.repo.task_prefix },
            "since": self.since,
            "tasks_created": self.tasks_created.iter().map(|t| serde_json::json!({
                "ref": format!("{}-{}", self.repo.task_prefix, t.seq),
                "priority": t.priority, "kind": t.kind, "title": t.title,
            })).collect::<Vec<_>>(),
            "tasks_closed": self.tasks_closed.iter().map(|t| serde_json::json!({
                "ref": format!("{}-{}", self.repo.task_prefix, t.seq),
                "priority": t.priority, "kind": t.kind, "title": t.title,
            })).collect::<Vec<_>>(),
            "digests": self.digests.iter().map(|d| serde_json::json!({
                "at": d.created_at, "agent": d.agent_name, "summary": d.summary,
            })).collect::<Vec<_>>(),
            "corrections": self.corrections,
            "reinforcements": self.reinforcements,
            "stats": {
                "prompts": self.prompts,
                "digests": self.digests_count,
                "similarity_hits": self.similarity_hits,
                "cache_hits": self.cache_hits,
                "cache_total": self.cache_total,
                "redactions": self.redactions,
            }
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
