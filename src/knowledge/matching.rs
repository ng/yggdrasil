//! Deterministic predicates matching the legacy PostgreSQL retrieval contract.
use super::document::Profile;
use anyhow::{Result, bail};
use uuid::Uuid;

#[derive(Default)]
pub struct Filters<'a> {
    pub repo: Option<Uuid>,
    pub file: Option<&'a str>,
    pub rule: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub kind: Option<&'a str>,
}

/// Compile SQL LIKE after translate('*?', '%_'), including pre-existing SQL
/// wildcards, backslash escapes, Unicode characters and directory separators.
pub fn file_pattern(pattern: &str) -> Result<regex::Regex> {
    let mut chars = pattern.chars().map(|c| match c {
        '*' => '%',
        '?' => '_',
        c => c,
    });
    let mut expression = String::from("\\A(?s:");
    while let Some(c) = chars.next() {
        match c {
            '%' => expression.push_str(".*"),
            '_' => expression.push('.'),
            '\\' => match chars.next() {
                Some(c) => expression.push_str(&regex::escape(&c.to_string())),
                None => bail!("LIKE pattern ends with escape character"),
            },
            c => expression.push_str(&regex::escape(&c.to_string())),
        }
    }
    expression.push_str(")\\z");
    Ok(regex::Regex::new(&expression)?)
}

fn jsonb_text(value: &serde_json::Value, nested: bool) -> String {
    use serde_json::Value;
    match value {
        Value::String(s) if !nested => s.clone(),
        Value::Number(n) => {
            use std::str::FromStr;
            sqlx::types::BigDecimal::from_str(&n.to_string())
                .expect("JSON numbers are decimals")
                .to_plain_string()
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|v| jsonb_text(v, true))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(items) => {
            // PostgreSQL JSONB orders keys by UTF-8 byte length then byte value.
            let mut entries: Vec<_> = items.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(k, v)| format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        jsonb_text(v, true)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        _ => value.to_string(),
    }
}

/// Activation and freshness are checked by Document::eligible before using
/// these scope predicates. Missing filters intentionally retain legacy meaning.
pub fn matches(profile: &Profile, filters: &Filters<'_>) -> Result<bool> {
    matches_with(profile, filters, |file, pattern| {
        Ok(file_pattern(pattern)?.is_match(file))
    })
}

/// Memoize only the file predicate within one immutable query. Scope, ownership,
/// activation and current document bytes remain independently checked by callers.
/// Bounded keys avoid retaining arbitrary corpora or large patterns across reads.
pub(crate) struct Matcher<'a, 'b> {
    filters: &'a Filters<'b>,
    files: std::collections::HashMap<String, bool>,
    bytes: usize,
}
impl<'a, 'b> Matcher<'a, 'b> {
    pub fn new(filters: &'a Filters<'b>) -> Self {
        Self {
            filters,
            files: std::collections::HashMap::new(),
            bytes: 0,
        }
    }
    pub fn matches(&mut self, profile: &Profile) -> Result<bool> {
        matches_with(profile, self.filters, |file, pattern| {
            if let Some(result) = self.files.get(pattern) {
                return Ok(*result);
            }
            let result = file_pattern(pattern)?.is_match(file);
            if self.files.len() < 256 && self.bytes.saturating_add(pattern.len()) <= 64 * 1024 {
                self.bytes += pattern.len();
                self.files.insert(pattern.to_owned(), result);
            }
            Ok(result)
        })
    }
}

fn matches_with(
    profile: &Profile,
    filters: &Filters<'_>,
    mut file_matches: impl FnMut(&str, &str) -> Result<bool>,
) -> Result<bool> {
    if filters.repo.is_some() && profile.repo.is_some() && profile.repo != filters.repo {
        return Ok(false);
    }
    if let (Some(file), Some(pattern)) = (filters.file, &profile.file_glob) {
        if !file_matches(file, pattern)? {
            return Ok(false);
        }
    }
    if filters.rule.is_some() && profile.rule_id.as_deref() != filters.rule {
        return Ok(false);
    }
    for (key, filter) in [("agent", filters.agent), ("kind", filters.kind)] {
        if let (Some(value), Some(filter)) =
            (profile.scope_tags.get(key).filter(|v| !v.is_null()), filter)
        {
            if jsonb_text(value, false) != filter {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// More-specific rules first, then newest first, then stable UUID ordering.
pub fn compare(a: &Profile, b: &Profile) -> std::cmp::Ordering {
    let specificity =
        |p: &Profile| usize::from(p.file_glob.is_some()) + usize::from(p.rule_id.is_some());
    specificity(b)
        .cmp(&specificity(a))
        .then_with(|| b.created_at.cmp(&a.created_at))
        .then_with(|| a.id.cmp(&b.id))
}

pub fn note_scope(repo: Option<Uuid>, filter: Option<Uuid>, all: bool) -> bool {
    all || repo.is_none() || repo == filter
}

#[cfg(test)]
mod query_tests {
    use super::*;
    fn profile() -> Profile {
        crate::knowledge::document::Document::parse(include_str!(
            "../../tests/fixtures/knowledge/rule.md"
        ))
        .unwrap()
        .profile()
        .unwrap()
        .unwrap()
    }
    #[test]
    fn repeated_patterns_do_not_cache_other_scope_dimensions_or_cross_queries() {
        let filters = Filters {
            file: Some("src/a.rs"),
            agent: Some("worker"),
            ..Filters::default()
        };
        let mut matcher = Matcher::new(&filters);
        let mut p = profile();
        p.file_glob = Some("src/*.rs".into());
        p.scope_tags
            .insert("agent".into(), serde_json::json!("worker"));
        assert!(matcher.matches(&p).unwrap());
        p.scope_tags
            .insert("agent".into(), serde_json::json!("other"));
        assert!(!matcher.matches(&p).unwrap());
        p.scope_tags
            .insert("agent".into(), serde_json::json!("worker"));
        assert!(matcher.matches(&p).unwrap());
        let other = Filters {
            file: Some("other/a.rs"),
            ..Filters::default()
        };
        assert!(!Matcher::new(&other).matches(&p).unwrap());
        for _ in 0..2 {
            p.file_glob = Some("invalid\\".into());
            assert!(matcher.matches(&p).is_err());
        }
    }
    #[test]
    fn capacity_limits_preserve_matching_and_invalid_pattern_errors() {
        let filters = Filters {
            file: Some("src/a.rs"),
            ..Filters::default()
        };
        let mut matcher = Matcher::new(&filters);
        let mut p = profile();
        for index in 0..1000 {
            p.file_glob = Some(format!("src/{index}.rs"));
            assert!(!matcher.matches(&p).unwrap());
        }
        assert!(matcher.files.len() <= 256 && matcher.bytes <= 64 * 1024);
        p.file_glob = Some("src/?.rs".into());
        assert!(matcher.matches(&p).unwrap());
        p.file_glob = Some("invalid\\".into());
        assert!(matcher.matches(&p).is_err());
    }
}
