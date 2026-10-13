use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::{
    knowledge::{
        document::{Profile, Scope, Source, State},
        matching::{self, Filters},
    },
    models::{learning::Learning, memory::Memory},
};

fn profile(row: Learning) -> Profile {
    Profile {
        schema_version: 1,
        id: row.learning_id,
        scope: if row.repo_id.is_some() {
            Scope::Repo
        } else {
            Scope::Global
        },
        repo: row.repo_id,
        legacy_repo_id: row.repo_id,
        user_id: None,
        created_by: row.created_by,
        created_at: row.created_at,
        context: row.context,
        file_glob: row.file_glob,
        rule_id: row.rule_id,
        scope_tags: serde_json::from_value(row.scope_tags).unwrap(),
        state: if row.status == "active" {
            State::Active
        } else {
            State::Pending
        },
        source: if row.source == "manual" {
            Source::Manual
        } else {
            Source::Proposed
        },
        approval: None,
        extra: BTreeMap::new(),
    }
}

#[test]
fn offline_matching_passes_the_same_fixtures_as_legacy_sql() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
    let profiles: Vec<_> = serde_json::from_value::<Vec<Learning>>(fixture["rows"].clone())
        .unwrap()
        .into_iter()
        .map(profile)
        .collect();
    for case in fixture["cases"].as_array().unwrap() {
        let filters = Filters {
            repo: case["repo"].as_str().map(|s| Uuid::parse_str(s).unwrap()),
            file: case["file"].as_str(),
            rule: case["rule"].as_str(),
            agent: case["agent"].as_str(),
            kind: case["kind"].as_str(),
        };
        let mut selected: Vec<_> = profiles
            .iter()
            .filter(|p| p.state == State::Active && matching::matches(p, &filters).unwrap())
            .collect();
        selected.sort_by(|a, b| matching::compare(a, b));
        let expected: Vec<u128> = serde_json::from_value(case["ids"].clone()).unwrap();
        assert_eq!(
            selected.iter().map(|p| p.id.as_u128()).collect::<Vec<_>>(),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn note_scope_and_prime_limit_match_legacy_fixtures() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/notes.json")).unwrap();
    let rows: Vec<Memory> = serde_json::from_value(fixture["rows"].clone()).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let repo = case["repo"].as_str().map(|s| Uuid::parse_str(s).unwrap());
        let mut selected: Vec<_> = rows
            .iter()
            .filter(|r| matching::note_scope(r.repo_id, repo, case["all"].as_bool().unwrap()))
            .collect();
        selected.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        selected.truncate(case["limit"].as_u64().unwrap() as usize);
        let expected: Vec<u128> = serde_json::from_value(case["ids"].clone()).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|r| r.memory_id.as_u128() & 0xffffffffffff)
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn like_patterns_match_unicode_newlines_escapes_and_literal_regex_syntax() {
    for (pattern, text, expected) in [
        ("*", "a/b\nλ", true),
        ("?", "λ", true),
        ("?", "ab", false),
        (r"\*", "%", true),
        (r"\?", "_", true),
        (r"\%", "%", true),
        (r"\_", "x", false),
        ("a[bc].rs", "ab.rs", false),
        ("a[bc].rs", "a[bc].rs", true),
        ("file", "file\n", false),
    ] {
        assert_eq!(
            matching::file_pattern(pattern).unwrap().is_match(text),
            expected,
            "{pattern:?} {text:?}"
        );
    }
    assert!(matching::file_pattern("trailing\\").is_err());
}

#[test]
fn equal_specificity_and_timestamp_are_ordered_by_uuid() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
    let mut a = profile(serde_json::from_value(fixture["rows"][0].clone()).unwrap());
    let mut b = a.clone();
    a.id = Uuid::from_u128(1);
    b.id = Uuid::from_u128(2);
    assert_eq!(matching::compare(&a, &b), std::cmp::Ordering::Less);
}
