//! `ygg learn` — scoped rule capture with deterministic match.
//!
//! See ADR 0015 and model/learning.rs. This is the orchestration-layer
//! replacement for fuzzy-similarity retrieval of durable rules. Scope
//! tuples are `(repo_id, file_glob, rule_id)`; lookups are SQL predicates,
//! not cosine thresholds.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::learning::LearningRepo;
use uuid::Uuid;

pub async fn create(
    pool: &sqlx::PgPool,
    text: &str,
    global: bool,
    file_glob: Option<&str>,
    rule_id: Option<&str>,
    context: Option<&str>,
    agent_name: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if global {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let created_by: Option<Uuid> =
        sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_name = $1")
            .bind(agent_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let learning = LearningRepo::new(pool)
        .create(repo_id, file_glob, rule_id, text, context, created_by)
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&learning)?);
        return Ok(());
    }
    let scope = match (repo_id, file_glob, rule_id) {
        (None, _, _) => "global".to_string(),
        (Some(_), Some(g), Some(r)) => format!("repo · {g} · {r}"),
        (Some(_), Some(g), None) => format!("repo · {g}"),
        (Some(_), None, Some(r)) => format!("repo · rule={r}"),
        (Some(_), None, None) => "repo".to_string(),
    };
    println!("Learned [{}] {}", scope, short(text, 100));
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    file_path: Option<&str>,
    rule_id: Option<&str>,
    all_repos: bool,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id)
    };
    let rows = LearningRepo::new(pool)
        .list_matching(repo_id, file_path, rule_id)
        .await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": rows.len(),
                "results": rows,
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No learnings match.");
        return Ok(());
    }
    for r in &rows {
        let scope = match (&r.repo_id, &r.file_glob, &r.rule_id) {
            (None, _, _) => "global".to_string(),
            (Some(_), Some(g), Some(id)) => format!("{g} · {id}"),
            (Some(_), Some(g), None) => g.to_string(),
            (Some(_), None, Some(id)) => format!("rule={id}"),
            (Some(_), None, None) => "repo".to_string(),
        };
        let applied = if r.applied_count > 0 {
            format!(" [×{}]", r.applied_count)
        } else {
            String::new()
        };
        println!("  · [{}]{} {}", scope, applied, short(&r.text, 100));
    }
    Ok(())
}

pub async fn delete(pool: &sqlx::PgPool, learning_id: Uuid) -> Result<(), anyhow::Error> {
    LearningRepo::new(pool).delete(learning_id).await?;
    println!("deleted {learning_id}");
    Ok(())
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
