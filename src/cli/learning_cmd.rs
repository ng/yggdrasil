//! `ygg learn` — scoped rule capture with deterministic match.
//!
//! See ADR 0015 and model/learning.rs. This is the orchestration-layer
//! replacement for fuzzy-similarity retrieval of durable rules. Scope
//! tuples are `(repo_id, file_glob, rule_id)`; lookups are SQL predicates,
//! not cosine thresholds.

use uuid::Uuid;

pub fn parse_scope_tags(scopes: &[String]) -> Result<serde_json::Value, anyhow::Error> {
    let mut map = serde_json::Map::new();
    for s in scopes {
        if s == "global" {
            continue;
        }
        if let Some((raw_key, raw_val)) = s.split_once('=') {
            let key = raw_key.trim();
            let val = raw_val.trim();
            if val.is_empty() {
                anyhow::bail!("invalid --scope '{s}': value cannot be empty");
            }
            match key {
                "agent" | "kind" => {
                    map.insert(key.to_string(), serde_json::Value::String(val.to_string()));
                }
                _ => {
                    anyhow::bail!(
                        "invalid --scope key '{key}', expected agent=<name> or kind=<task-kind>"
                    );
                }
            }
        } else {
            anyhow::bail!(
                "invalid --scope '{s}', expected global, agent=<name>, or kind=<task-kind>"
            );
        }
    }
    Ok(serde_json::Value::Object(map))
}

/// Optional knowledge cannot undo a successful task claim. A selected or invalid
/// binding never falls back to frozen SQL content. Keep both leases until stdout.
pub async fn print_for_claim(
    pool: &sqlx::PgPool,
    repo: Uuid,
    files: &[String],
    agent: &str,
    kind: &str,
) {
    use crate::knowledge::runtime::Context;
    let result = async {
        match Context::from_environment(std::env::vars().collect())? {
            Some(context) => {
                let (repo, lease) = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    context.task_scope(pool, repo),
                )
                .await??;
                let emission = local::surface_for_files(&context, repo, files, agent, kind)?;
                for line in &emission.lines {
                    println!("{line}");
                }
                crate::knowledge::usage::after_emission(&context, lease, &emission.applications)
                    .await;
            }
            None => {
                eprintln!("knowledge: not initialized; run `ygg init` to create the OKF corpus");
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        // Do not expose connection strings or credentials from driver errors.
        eprintln!("knowledge: task rules unavailable; claim succeeded");
    }
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
        let tags = parse_scope_tags(&["global".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({}));
    }

    #[test]
    fn parse_scope_tags_agent() {
        let tags = parse_scope_tags(&["agent=linter".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"agent": "linter"}));
    }

    #[test]
    fn parse_scope_tags_kind() {
        let tags = parse_scope_tags(&["kind=bug".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"kind": "bug"}));
    }

    #[test]
    fn parse_scope_tags_combined() {
        let tags = parse_scope_tags(&["agent=ci".to_string(), "kind=feature".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"agent": "ci", "kind": "feature"}));
    }

    #[test]
    fn parse_scope_tags_unknown_key_errors() {
        let err = parse_scope_tags(&["agnt=foo".to_string()]);
        assert!(err.is_err(), "unknown key should fail");
    }

    #[test]
    fn parse_scope_tags_no_equals_errors() {
        let err = parse_scope_tags(&["bogus".to_string()]);
        assert!(err.is_err(), "token without '=' should fail");
    }

    #[test]
    fn parse_scope_tags_empty_value_errors() {
        let err = parse_scope_tags(&["agent=".to_string()]);
        assert!(err.is_err(), "empty value should fail");
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod local;
