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

/// Surface scoped learnings whose `file_glob` matches any of the given paths
/// (yggdrasil-82). Returns formatted lines suitable for direct stdout / hook
/// injection. Increments each surfaced learning's `applied_count` so the
/// usage telemetry reflects real hits, not just creation.
///
/// Repo-scope and global learnings (`file_glob IS NULL`) are included
/// unconditionally — the surface function is the orchestration-layer
/// replacement for fuzzy similarity retrieval of durable rules per ADR 0015.
pub async fn surface_for_files(
    pool: &sqlx::PgPool,
    repo_id: Option<Uuid>,
    files: &[String],
) -> Result<Vec<String>, anyhow::Error> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let repo = LearningRepo::new(pool);

    // Pass 1: per-file matches. NULL file_glob counts as "applies to any
    // file in this repo" — list_matching includes those automatically.
    for f in files {
        let rows = repo.list_matching(repo_id, Some(f), None).await?;
        for l in rows {
            if !seen.insert(l.learning_id) {
                continue;
            }
            out.push(format_learning_line(&l));
            let _ = repo.increment_applied(l.learning_id).await;
        }
    }

    // Pass 2: also surface repo-wide learnings (file_glob IS NULL) even when
    // no file paths were detected — they're "always relevant in this repo."
    // Skip both glob-scoped and rule-scoped rows here: those need an explicit
    // file path / rule id match to surface, and `list_matching(_, None, None)`
    // happily returns them otherwise.
    if files.is_empty() {
        let rows = repo.list_matching(repo_id, None, None).await?;
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

/// Best-effort file-path extractor for free-text task fields. Matches tokens
/// that look like `<path>.<ext>` where `ext` is 2–5 alpha chars, capturing
/// common forms (`src/main.rs`, `Cargo.toml`, `terraform/*.tf`,
/// `docs/orchestration.md`). Deliberately conservative; false negatives
/// beat false positives in a learning-surface context.
pub fn extract_file_mentions(text: &str) -> Vec<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    // Allow multi-dot basenames (e.g. `vite.config.ts`, `tsconfig.build.json`)
    // by letting the basename consume an optional run of dot-separated
    // segments before the final 2–5-letter extension. The directory prefix
    // before the basename keeps the same character class as before.
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

fn format_learning_line(l: &crate::models::learning::Learning) -> String {
    let scope = match (&l.repo_id, &l.file_glob, &l.rule_id) {
        (None, _, _) => "global".to_string(),
        (Some(_), Some(g), Some(id)) => format!("{g} · {id}"),
        (Some(_), Some(g), None) => g.to_string(),
        (Some(_), None, Some(id)) => format!("rule={id}"),
        (Some(_), None, None) => "repo".to_string(),
    };
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
        // "1.0.220" / "1.40" are version strings, not file paths.
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
}
