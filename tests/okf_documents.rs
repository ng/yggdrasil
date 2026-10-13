use chrono::{TimeZone, Utc};
use serde_yaml_ng::Value;
use uuid::Uuid;
use ygg::knowledge::document::{
    ActivationKind, Document, MAX_DOCUMENT_BYTES, MAX_FRONTMATTER_BYTES,
};

fn rule() -> Document {
    Document::parse(include_str!("fixtures/knowledge/rule.md")).unwrap()
}
fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}
fn corpus() -> Uuid {
    Uuid::from_u128(42)
}
fn approved() -> Document {
    let mut doc = rule();
    doc.activate(
        corpus(),
        ActivationKind::Reviewed,
        Some(Uuid::from_u128(1)),
        Some(now()),
    )
    .unwrap();
    doc
}

#[test]
fn round_trip_preserves_unknown_fields_and_exact_body() {
    let doc = rule();
    let round_trip = Document::parse(&doc.serialize().unwrap()).unwrap();
    assert_eq!(doc, round_trip);
    let mut doc = approved();
    doc.reset_import_activation().unwrap();
    assert_eq!(
        doc.metadata["x-producer"],
        round_trip.metadata["x-producer"]
    );
    assert_eq!(doc.metadata["ygg"]["x-future-policy"], "preserved");
    assert_eq!(doc.body, round_trip.body);
    let crlf = "---\r\ntype: Note\r\n---\r\n\r\nUnchanged\r\n";
    assert_eq!(Document::parse(crlf).unwrap().body, "\r\nUnchanged\r\n");
    assert_eq!(Document::parse("---\ntype: Note\n---").unwrap().body, "");
}

#[test]
fn generic_okf_and_descriptive_verification_never_activate() {
    let generic = Document::parse("---\ntype: Future Type\nstatus: stable\nverified: {by: 'human:someone', at: 2026-10-08T00:00:00Z}\n---\nDo this.").unwrap();
    assert!(!generic.eligible(Some(corpus()), now()).unwrap());
    assert!(generic.profile().unwrap().is_none());
    let mut pending = rule();
    pending
        .metadata
        .insert("verified".into(), Value::String("human:someone".into()));
    assert!(!pending.eligible(Some(corpus()), now()).unwrap());
    assert!(
        pending
            .activate(corpus(), ActivationKind::Manual, None, Some(now()))
            .is_err()
    );
}

#[test]
fn activation_is_bound_to_content_scope_and_trust_domain() {
    let original = approved();
    assert!(original.eligible(Some(corpus()), now()).unwrap());
    assert!(!original.eligible(None, now()).unwrap());
    assert!(!original.eligible(Some(Uuid::from_u128(43)), now()).unwrap());
    let mut edited = original.clone();
    edited.body.push('!');
    assert!(!edited.eligible(Some(corpus()), now()).unwrap());
    for (key, replacement) in [
        ("context", Value::String("new context".into())),
        ("file_glob", Value::String("*".into())),
        ("rule_id", Value::Null),
        ("user_id", Value::String("another-user".into())),
        ("repo", Value::String(Uuid::from_u128(9).to_string())),
        (
            "scope_tags",
            serde_yaml_ng::from_str("{agent: someone}").unwrap(),
        ),
    ] {
        let mut edited = original.clone();
        edited.metadata.get_mut("ygg").unwrap()[key] = replacement;
        assert!(!edited.eligible(Some(corpus()), now()).unwrap(), "{key}");
    }
    let mut display_edit = original.clone();
    display_edit
        .metadata
        .insert("title".into(), "Better title".into());
    display_edit.metadata.get_mut("ygg").unwrap()["applied_count"] = Value::Number(9.into());
    assert!(display_edit.eligible(Some(corpus()), now()).unwrap());
    let mut imported = original;
    imported.reset_import_activation().unwrap();
    assert!(!imported.eligible(Some(corpus()), now()).unwrap());
    assert!(imported.profile().unwrap().unwrap().approval.is_none());
}

#[test]
fn stale_and_deprecated_rules_do_not_fire() {
    let mut doc = approved();
    doc.metadata
        .insert("stale_after".into(), "2026-10-08T12:00:00Z".into());
    assert!(!doc.eligible(Some(corpus()), now()).unwrap());
    doc.metadata.remove("stale_after");
    doc.metadata.insert("status".into(), "deprecated".into());
    assert!(!doc.eligible(Some(corpus()), now()).unwrap());
    doc.metadata.insert("status".into(), "stable".into());
    assert!(doc.eligible(Some(corpus()), now()).unwrap());
}

#[test]
fn legacy_activation_preserves_missing_evidence() {
    let mut doc = rule();
    doc.activate(corpus(), ActivationKind::Legacy, None, None)
        .unwrap();
    assert!(doc.eligible(Some(corpus()), now()).unwrap());
    let approval = doc.profile().unwrap().unwrap().approval.unwrap();
    assert!(approval.actor.is_none());
    assert!(approval.at.is_none());
    assert!(!doc.metadata.contains_key("verified"));
}

#[test]
fn malformed_and_oversized_documents_are_rejected() {
    for text in [
        "plain",
        "---\ntype: Note",
        "---\ntitle: Missing type\n---\n",
        "---\ntype: [Not, String]\n---\n",
        "---\ntype: Note\ntype: Rule\n---\n",
    ] {
        assert!(Document::parse(text).is_err(), "{text}");
    }
    assert!(Document::parse(&"x".repeat(MAX_DOCUMENT_BYTES + 1)).is_err());
    assert!(
        Document::parse(&format!(
            "---\ntype: Note\na: {}\n---\n",
            "x".repeat(MAX_FRONTMATTER_BYTES)
        ))
        .is_err()
    );
    let deeply_nested = format!(
        "---\ntype: Note\na: {}0{}\n---\n",
        "[".repeat(150),
        "]".repeat(150)
    );
    assert!(Document::parse(&deeply_nested).is_err());
    let mut ambiguous = rule();
    ambiguous.metadata.get_mut("ygg").unwrap()["scope"] = "global".into();
    assert!(ambiguous.profile().is_err());
}

#[test]
fn canonical_digest_matches_pinned_fixture() {
    let doc = rule();
    assert_eq!(
        String::from_utf8(doc.approval_input().unwrap()).unwrap(),
        include_str!("fixtures/knowledge/approval-input.json").trim_end()
    );
    assert_eq!(
        doc.approval_digest().unwrap(),
        include_str!("fixtures/knowledge/approval-digest.txt").trim()
    );
}

#[test]
fn yaml_alias_expansion_is_bounded_and_unknown_tags_survive() {
    let yaml = format!(
        "---\ntype: Note\na: &a {}\nb: [{}]\n---\n",
        "x".repeat(1024),
        vec!["*a"; 300].join(",")
    );
    assert!(Document::parse(&yaml).is_err());
    let tagged = Document::parse("---\ntype: Note\nfuture: !Custom {answer: 42}\n---\n").unwrap();
    assert_eq!(
        tagged,
        Document::parse(&tagged.serialize().unwrap()).unwrap()
    );
    let aliased = Document::parse("---\ntype: Note\na: &a [1, 2]\nb: *a\n---\n").unwrap();
    assert_eq!(aliased.metadata["a"], aliased.metadata["b"]);
}
