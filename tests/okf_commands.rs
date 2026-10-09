#![cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeMap;
use uuid::Uuid;
use ygg::knowledge::{
    identity::IdentityRegistry,
    runtime::{Phase, SELECTION_FILE},
    store::KnowledgeStore,
};

#[path = "support/okf.rs"]
mod support;
use support::*;

#[test]
fn ordinary_remember_uses_selected_okf_offline_and_shares_worktree_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, repo, agent) = fixture(root);
    select(root, &binding);
    let note = json(
        command(root, &root.join("repo"))
            .args([" exact note Ω ", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(note["repo_id"], repo.to_string());
    assert_eq!(note["created_by"], agent.to_string());
    assert_eq!(note["text"], "exact note Ω");
    assert_eq!(note.as_object().unwrap().len(), 5);
    let global = json(
        command(root, root)
            .args(["global", "--global", "--json"])
            .output()
            .unwrap(),
    );
    assert!(global["repo_id"].is_null());
    let list = json(
        command(root, &root.join("worktree"))
            .args(["--list", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["count"], 2);
    assert_eq!(list["results"][1], note);
    assert!(
        !root.join("data").exists(),
        "knowledge access created managed/database state"
    );
    let failed = command(root, root)
        .arg("must not become global")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let list = json(
        command(root, root)
            .args(["--list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["count"], 2);
}
#[test]
fn fenced_invalid_or_unmapped_selection_never_falls_back_or_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = fixture(root);
    for fault in 0..4 {
        binding.phase = if fault == 0 {
            Phase::Fenced
        } else {
            Phase::Okf
        };
        binding.minimum_client = if fault == 1 { 999 } else { 1 };
        select(root, &binding);
        let mut cmd = command(root, &root.join("repo"));
        if fault == 2 {
            cmd.env("YGG_USER", "unmapped");
        }
        if fault == 3 {
            std::fs::write(root.join("policy").join(SELECTION_FILE), "broken").unwrap();
        }
        let output = cmd.args(["must not write", "--json"]).output().unwrap();
        assert!(!output.status.success());
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("requires DATABASE_URL"),
            "fell back to SQL"
        );
        assert!(
            KnowledgeStore::open(&root.join("bundle"), false)
                .unwrap()
                .snapshot()
                .documents
                .is_empty()
        );
        assert!(!root.join("data").exists());
    }
}

#[test]
fn ordinary_learn_preserves_lifecycle_and_blocks_unauthorized_approval_offline() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, repo, agent) = fixture(root);
    select(root, &binding);
    let proposal = json(
        learn(root)
            .args([
                "propose",
                "Exact rule Ω\n",
                "--file-glob",
                "src/*.rs",
                "--context",
                "why",
                "--scope",
                "kind=bug",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let id = proposal["learning_id"].as_str().unwrap();
    assert_eq!(proposal["repo_id"], repo.to_string());
    assert_eq!(proposal["text"], "Exact rule Ω\n");
    assert_eq!(proposal["status"], "pending");
    assert_eq!(proposal["source"], "proposed");
    assert_eq!(proposal["applied_count"], 0);
    assert!(proposal["approved_at"].is_null());
    assert_eq!(
        json(learn(root).args(["list", "--json"]).output().unwrap())["count"],
        0
    );
    assert!(
        !learn(root)
            .args(["approve", id])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !learn(root)
            .args(["approve", id, "--agent", "unknown"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let registry = IdentityRegistry::open(&root.join("policy"), false).unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.approval_leads.insert(agent);
    registry.replace(&revision, &policy).unwrap();
    success(learn(root).args(["approve", id]).output().unwrap());
    let list = json(
        learn(root)
            .args(["list", "--file", "src/test.rs", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["count"], 1);
    assert_eq!(list["results"][0]["approved_by"], agent.to_string());
    assert_eq!(
        list["results"][0]["scope_tags"],
        serde_json::json!({"kind":"bug"})
    );
    assert_eq!(
        json(
            learn(root)
                .args(["list", "--file", "other.txt", "--json"])
                .output()
                .unwrap()
        )["count"],
        0
    );
    assert!(
        !learn(root)
            .args(["reject", id])
            .output()
            .unwrap()
            .status
            .success()
    );
    // External covered edits revoke activation immediately, without trusting an index.
    let store = KnowledgeStore::open(&root.join("bundle"), false).unwrap();
    let mut doc = store.find(Uuid::parse_str(id).unwrap()).unwrap().unwrap();
    doc.document.body = "changed without approval".into();
    std::fs::write(
        root.join("bundle").join(doc.key.relative_path()),
        doc.document.serialize().unwrap(),
    )
    .unwrap();
    assert_eq!(
        json(learn(root).args(["list", "--json"]).output().unwrap())["count"],
        0
    );
    assert_eq!(
        json(learn(root).args(["pending", "--json"]).output().unwrap())["count"],
        1
    );
    success(
        learn(root)
            .args(["reject", id, "--reason", "changed"])
            .output()
            .unwrap(),
    );
    let manual = json(
        learn(root)
            .args(["create", "manual", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(manual["status"], "active");
    assert!(manual["approved_at"].is_null());
    assert!(manual["approved_by"].is_null());
    // Untrusted corpus remains explicitly browseable, but service injection still checks trust.
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = false;
    registry.replace(&revision, &policy).unwrap();
    assert_eq!(
        json(learn(root).args(["list", "--json"]).output().unwrap())["count"],
        1
    );
    let pending = json(
        learn(root)
            .args(["create", "human approval", "--pending", "--json"])
            .output()
            .unwrap(),
    );
    success(
        learn(root)
            .env_remove("YGG_AGENT_NAME")
            .args(["approve", pending["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    let note = json(
        command(root, &root.join("repo"))
            .args(["note", "--json"])
            .output()
            .unwrap(),
    );
    assert!(
        !learn(root)
            .args(["delete", note["memory_id"].as_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success()
    );
    success(
        learn(root)
            .args(["delete", manual["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(!root.join("data").exists());
}

#[test]
fn imported_usage_is_preserved_and_telemetry_metadata_never_blocks_rule_creation() {
    use ygg::knowledge::{
        legacy::{Usage, import_learning},
        runtime::UsageSnapshot,
        store::ExpectedRevision,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = fixture(root);
    select(root, &binding);
    let value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/learnings.json")).unwrap();
    let mut row: ygg::models::learning::Learning =
        serde_json::from_value(value["rows"][0].clone()).unwrap();
    row.applied_count = 42;
    row.last_applied_at = Some(row.created_at);
    let (doc, usage) = import_learning(&row, "legacy-user", &binding.mappings).unwrap();
    KnowledgeStore::open(&root.join("bundle"), false)
        .unwrap()
        .put(&doc, ExpectedRevision::Absent)
        .unwrap();
    let failed = learn(root)
        .args(["list", "--all", "--json"])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "missing imported totals fabricated zero"
    );
    let snapshot = UsageSnapshot {
        version: 1,
        corpus_id: binding.mappings.corpus_id,
        totals: BTreeMap::from([(row.learning_id, Usage { ..usage })]),
    };
    let path = root.join("policy/usage-baseline.json");
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let list = json(
        learn(root)
            .args(["list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(list["results"][0]["applied_count"], 42);
    assert_eq!(
        list["results"][0]["last_applied_at"],
        serde_json::to_value(row.last_applied_at).unwrap()
    );
    let mut cached = serde_json::to_value(&snapshot).unwrap();
    cached["totals"][row.learning_id.to_string()]["applied_count"] = 43.into();
    std::fs::write(
        root.join("policy/usage-snapshot.json"),
        serde_json::to_vec(&cached).unwrap(),
    )
    .unwrap();
    assert_eq!(
        json(
            learn(root)
                .args(["list", "--all", "--json"])
                .output()
                .unwrap()
        )["results"][0]["applied_count"],
        43
    );
    cached["totals"][row.learning_id.to_string()]["applied_count"] = 40.into();
    std::fs::write(
        root.join("policy/usage-snapshot.json"),
        serde_json::to_vec(&cached).unwrap(),
    )
    .unwrap();
    assert_eq!(
        json(
            learn(root)
                .args(["list", "--all", "--json"])
                .output()
                .unwrap()
        )["results"][0]["applied_count"],
        42
    );
    std::fs::write(root.join("policy/usage-snapshot.json"), "broken telemetry").unwrap();
    let created = json(
        learn(root)
            .args(["propose", "still accepted", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(created["status"], "pending");
    let list = json(
        learn(root)
            .args(["list", "--all", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        list["results"][0]["applied_count"], 42,
        "broken optional cache erased baseline"
    );
    success(
        learn(root)
            .env_remove("YGG_AGENT_NAME")
            .args(["approve", created["learning_id"].as_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(!root.join("data").exists());
}
