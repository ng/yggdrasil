//! `ygg learn` — scoped rule capture with deterministic match.
//!
//! See ADR 0015 and model/learning.rs. This is the orchestration-layer
//! replacement for fuzzy-similarity retrieval of durable rules. Scope
//! tuples are `(repo_id, file_glob, rule_id)`; lookups are SQL predicates,
//! not cosine thresholds.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::learning::LearningRepo;
use uuid::Uuid;

pub fn parse_scope_tags(scopes: &[String]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for s in scopes {
        if s == "global" {
            continue;
        }
        if let Some((key, val)) = s.split_once('=') {
            match key {
                "agent" | "kind" => {
                    map.insert(key.to_string(), serde_json::Value::String(val.to_string()));
                }
                _ => {
                    eprintln!(
                        "warning: unknown scope key '{key}', expected agent=<name> or kind=<task-kind>"
                    );
                }
            }
        } else {
            eprintln!(
                "warning: unrecognized scope '{s}', expected global, agent=<name>, or kind=<task-kind>"
            );
        }
    }
    serde_json::Value::Object(map)
}

pub async fn create(
    pool: &sqlx::PgPool,
    text: &str,
    global: bool,
    file_glob: Option<&str>,
    rule_id: Option<&str>,
    context: Option<&str>,
    agent_name: &str,
    scope_tags: &serde_json::Value,
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
        .create(
            repo_id, file_glob, rule_id, text, context, created_by, scope_tags,
        )
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&learning)?);
        return Ok(());
    }
    let scope = format_scope_label(repo_id, file_glob, rule_id, scope_tags);
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
        .list_matching(repo_id, file_path, rule_id, None, None)
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
        let scope = format_scope_label(
            r.repo_id,
            r.file_glob.as_deref(),
            r.rule_id.as_deref(),
            &r.scope_tags,
        );
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

/// Surface scoped learnings whose `file_glob` matches any of the given paths
/// (yggdrasil-82). Returns formatted lines suitable for direct stdout / hook
/// injection. Increments each surfaced learning's `applied_count` so the
/// usage telemetry reflects real hits, not just creation.
pub async fn surface_for_files(
    pool: &sqlx::PgPool,
    repo_id: Option<Uuid>,
    files: &[String],
    agent_name: Option<&str>,
    task_kind: Option<&str>,
) -> Result<Vec<String>, anyhow::Error> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let repo = LearningRepo::new(pool);

    for f in files {
        let rows = repo
            .list_matching(repo_id, Some(f), None, agent_name, task_kind)
            .await?;
        for l in rows {
            if !seen.insert(l.learning_id) {
                continue;
            }
            out.push(format_learning_line(&l));
            let _ = repo.increment_applied(l.learning_id).await;
        }
    }

    if files.is_empty() {
        let rows = repo
            .list_matching(repo_id, None, None, agent_name, task_kind)
            .await?;
        for l in rows {
            if l.file_glob.is_some() || l.rule_id.is_some() {
                continue;
            }
            if !seen.insert(l.learning_id) {
                continue;
            }
            out.push(format_learning_line(&l));
            let _ = repo.increment_applied(l.learning_id).await;
        }
    }

    Ok(out)
}

/// Best-effort file-path extractor for free-text task fields.
pub fn extract_file_mentions(text: &str) -> Vec<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static FILE_PATTERN: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?:[a-zA-Z0-9_./*\-]+/)?(?:[a-zA-Z0-9_*\-]+(?:\.[a-zA-Z0-9_*\-]+)*)\.[a-zA-Z]{2,5}",
        )
        .expect("file-mention regex")
    });
    let mut found: Vec<String> = FILE_PATTERN
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect();
    found.sort();
    found.dedup();
    found
}

fn scope_tag_fragments(scope_tags: &serde_json::Value) -> Vec<String> {
    let mut frags = Vec::new();
    if let Some(obj) = scope_tags.as_object() {
        if let Some(a) = obj.get("agent").and_then(|v| v.as_str()) {
            frags.push(format!("agent={a}"));
        }
        if let Some(k) = obj.get("kind").and_then(|v| v.as_str()) {
            frags.push(format!("kind={k}"));
        }
    }
    frags
}

fn format_scope_label(
    repo_id: Option<Uuid>,
    file_glob: Option<&str>,
    rule_id: Option<&str>,
    scope_tags: &serde_json::Value,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (repo_id, file_glob, rule_id) {
        (None, _, _) => parts.push("global".to_string()),
        (Some(_), Some(g), Some(r)) => {
            parts.push(g.to_string());
            parts.push(r.to_string());
        }
        (Some(_), Some(g), None) => parts.push(g.to_string()),
        (Some(_), None, Some(r)) => parts.push(format!("rule={r}")),
        (Some(_), None, None) => parts.push("repo".to_string()),
    }
    parts.extend(scope_tag_fragments(scope_tags));
    parts.join(" · ")
}

fn format_learning_line(l: &crate::models::learning::Learning) -> String {
    let scope = format_scope_label(
        l.repo_id,
        l.file_glob.as_deref(),
        l.rule_id.as_deref(),
        &l.scope_tags,
    );
    format!("[ygg learning · {scope}] {}", short(&l.text, 200))
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_common_paths() {
        let t = "Edit src/main.rs and update Cargo.toml; also touch docs/orchestration.md.";
        let found = extract_file_mentions(t);
        assert!(found.contains(&"src/main.rs".to_string()));
        assert!(found.contains(&"Cargo.toml".to_string()));
        assert!(found.contains(&"docs/orchestration.md".to_string()));
    }

    #[test]
    fn rejects_version_strings() {
        let t = "Bump serde to 1.0.220 and tokio to 1.40.";
        let found = extract_file_mentions(t);
        assert!(found.iter().all(|f| !f.starts_with("1.")));
    }

    #[test]
    fn handles_globs() {
        let t = "Run `cargo check`; touches migrations/*.sql and terraform/*.tf";
        let found = extract_file_mentions(t);
        assert!(found.iter().any(|f| f.contains("*.sql")));
        assert!(found.iter().any(|f| f.contains("*.tf")));
    }

    #[test]
    fn handles_multi_dot_basenames() {
        let t = "Update vite.config.ts and tsconfig.build.json before merging.";
        let found = extract_file_mentions(t);
        assert!(
            found.iter().any(|f| f == "vite.config.ts"),
            "should preserve the full multi-dot basename, got {found:?}"
        );
        assert!(
            found.iter().any(|f| f == "tsconfig.build.json"),
            "should preserve the full multi-dot basename, got {found:?}"
        );
    }

    #[test]
    fn parse_scope_tags_global() {
        let tags = parse_scope_tags(&["global".to_string()]);
        assert_eq!(tags, serde_json::json!({}));
    }

    #[test]
    fn parse_scope_tags_agent() {
        let tags = parse_scope_tags(&["agent=linter".to_string()]);
        assert_eq!(tags, serde_json::json!({"agent": "linter"}));
    }

    #[test]
    fn parse_scope_tags_kind() {
        let tags = parse_scope_tags(&["kind=bug".to_string()]);
        assert_eq!(tags, serde_json::json!({"kind": "bug"}));
    }

    #[test]
    fn parse_scope_tags_combined() {
        let tags = parse_scope_tags(&["agent=ci".to_string(), "kind=feature".to_string()]);
        assert_eq!(tags, serde_json::json!({"agent": "ci", "kind": "feature"}));
    }

    #[test]
    fn scope_label_with_tags() {
        let label = format_scope_label(
            Some(Uuid::nil()),
            Some("src/*.rs"),
            None,
            &serde_json::json!({"agent": "foo", "kind": "bug"}),
        );
        assert_eq!(label, "src/*.rs · agent=foo · kind=bug");
    }
}
