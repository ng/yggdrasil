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
    let created: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(note["created_at"].clone()).unwrap();
    assert_eq!(created.timestamp_subsec_nanos() % 1000, 0);

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
    let approved: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(list["results"][0]["approved_at"].clone()).unwrap();
    assert_eq!(approved.timestamp_subsec_nanos() % 1000, 0);

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
    let manual_id = Uuid::parse_str(manual["learning_id"].as_str().unwrap()).unwrap();
    let current = store.find(manual_id).unwrap().unwrap();
    ygg::knowledge::legacy::reverse_learning(
        &current.document,
        &ygg::knowledge::legacy::Usage {
            corpus_id: binding.mappings.corpus_id,
            document_id: manual_id,
            applied_count: 0,
            last_applied_at: None,
        },
        &binding.mappings,
    )
    .unwrap();

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

#[test]
fn local_fence_is_offline_resumable_and_preserves_conflicting_selection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = fixture(root);
    select(root, &binding);
    let original = std::fs::read(root.join("policy/runtime.json")).unwrap();
    let fence = |generation: &str| {
        app(root, root)
            .args([
                "knowledge",
                "fence-local",
                "--expected-generation",
                generation,
                "--json",
            ])
            .output()
            .unwrap()
    };
    assert!(!fence("3").status.success());
    assert_eq!(
        std::fs::read(root.join("policy/runtime.json")).unwrap(),
        original
    );
    assert!(!root.join("policy/local-fence-3.json").exists());
    let first = json(fence("2"));
    assert_eq!(first["source_generation"], 2);
    assert_eq!(first["corpus_id"], binding.mappings.corpus_id.to_string());
    let journal = std::fs::read(root.join("policy/local-fence-2.json")).unwrap();
    let fenced = std::fs::read(root.join("policy/runtime.json")).unwrap();
    let actual: ygg::knowledge::runtime::Binding = serde_json::from_slice(&fenced).unwrap();
    assert!(actual.phase == Phase::Fenced);
    assert_eq!(json(fence("2")), first);
    let output = command(root, root)
        .args(["must not write", "--global"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cutover is fenced"));
    assert!(!root.join("data").exists());
    // Recreate the durable state of a crash after intent but before selection.
    std::fs::write(root.join("policy/runtime.json"), &original).unwrap();
    assert_eq!(json(fence("2")), first);
    assert_eq!(
        std::fs::read(root.join("policy/runtime.json")).unwrap(),
        fenced
    );
    assert_eq!(
        std::fs::read(root.join("policy/local-fence-2.json")).unwrap(),
        journal
    );
    // Independent edits must never be replaced by replaying the retained intent.
    binding.agents.insert("independent".into(), Uuid::new_v4());
    select(root, &binding);
    let edited = std::fs::read(root.join("policy/runtime.json")).unwrap();
    assert!(!fence("2").status.success());
    assert_eq!(
        std::fs::read(root.join("policy/runtime.json")).unwrap(),
        edited
    );
    std::fs::write(root.join("policy/runtime.json"), &original).unwrap();
    // Replacing the corpus at the same path cannot inherit the old fencing proof.
    std::fs::rename(root.join("bundle"), root.join("saved-bundle")).unwrap();
    KnowledgeStore::open(&root.join("bundle"), true).unwrap();
    assert!(!fence("2").status.success());
    assert_eq!(
        std::fs::read(root.join("policy/runtime.json")).unwrap(),
        original
    );
}

#[test]
fn coordinated_local_fence_binds_retries_to_operation_and_participant() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = fixture(root);
    select(root, &binding);
    let selection = root.join("policy/runtime.json");
    let journal_path = root.join("policy/local-fence-2.json");
    let original = std::fs::read(&selection).unwrap();
    let operation = Uuid::new_v4().to_string();
    let participant = Uuid::new_v4().to_string();
    let fence = |extra: &[&str]| {
        app(root, root)
            .args([
                "knowledge",
                "fence-local",
                "--expected-generation",
                "2",
                "--json",
            ])
            .args(extra)
            .output()
            .unwrap()
    };
    let nil = Uuid::nil().to_string();
    for args in [
        vec!["--migration-operation", operation.as_str()],
        vec!["--participant", participant.as_str()],
        vec![
            "--migration-operation",
            nil.as_str(),
            "--participant",
            participant.as_str(),
        ],
        vec![
            "--migration-operation",
            operation.as_str(),
            "--participant",
            nil.as_str(),
        ],
    ] {
        assert!(!fence(&args).status.success());
        assert_eq!(std::fs::read(&selection).unwrap(), original);
        assert!(!journal_path.exists());
    }
    let args = [
        "--migration-operation",
        operation.as_str(),
        "--participant",
        participant.as_str(),
    ];
    let first = json(fence(&args));
    assert_eq!(first["coordinator"]["migration_operation"], operation);
    assert_eq!(first["coordinator"]["participant"], participant);
    let journal = std::fs::read(&journal_path).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&journal).unwrap()["version"],
        2
    );
    let fenced = std::fs::read(&selection).unwrap();
    assert_eq!(json(fence(&args)), first);
    let other = Uuid::new_v4().to_string();
    for wrong in [
        vec![],
        vec![
            "--migration-operation",
            other.as_str(),
            "--participant",
            participant.as_str(),
        ],
        vec![
            "--migration-operation",
            operation.as_str(),
            "--participant",
            other.as_str(),
        ],
    ] {
        assert!(!fence(&wrong).status.success());
        assert_eq!(std::fs::read(&selection).unwrap(), fenced);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
    }
    // Replay the state left by a crash after the durable intent was written.
    std::fs::write(&selection, &original).unwrap();
    assert_eq!(json(fence(&args)), first);
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
    binding.agents.insert("independent".into(), Uuid::new_v4());
    select(root, &binding);
    let edited = std::fs::read(&selection).unwrap();
    assert!(!fence(&args).status.success());
    assert_eq!(std::fs::read(&selection).unwrap(), edited);
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
}

#[test]
fn local_fence_drains_existing_selection_readers_before_publication() {
    use fs2::FileExt;
    use std::{os::unix::fs::OpenOptionsExt, sync::mpsc, time::Duration};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = fixture(root);
    select(root, &binding);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(root.join("policy/.selection.lock"))
        .unwrap();
    FileExt::lock_shared(&file).unwrap();
    let env = BTreeMap::from([
        ("HOME".to_string(), root.to_string_lossy().into_owned()),
        (
            "YGG_KNOWLEDGE_DIR".into(),
            root.join("bundle").to_string_lossy().into_owned(),
        ),
        (
            "YGG_KNOWLEDGE_POLICY_DIR".into(),
            root.join("policy").to_string_lossy().into_owned(),
        ),
    ]);
    let (config, _) = ygg::config::database::KnowledgeConfig::load(env).unwrap();
    let (started, ready) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        started.send(()).unwrap();
        done.send(ygg::knowledge::fence::local(&config, 2)).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(result.recv_timeout(Duration::from_millis(200)).is_err());
    assert!(!root.join("policy/local-fence-2.json").exists());
    let current: ygg::knowledge::runtime::Binding =
        serde_json::from_slice(&std::fs::read(root.join("policy/runtime.json")).unwrap()).unwrap();
    assert!(current.phase == Phase::Okf);
    drop(file);
    result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    let current: ygg::knowledge::runtime::Binding =
        serde_json::from_slice(&std::fs::read(root.join("policy/runtime.json")).unwrap()).unwrap();
    assert!(current.phase == Phase::Fenced);
}

#[test]
fn sql_host_preparation_fences_absent_selection_and_preserves_retry_evidence() {
    use ygg::knowledge::fence::{CoordinatorBinding, prepare_sql};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut binding, _, _) = fixture(root);
    let (config, _) = ygg::config::database::KnowledgeConfig::load(BTreeMap::from([
        ("HOME".into(), root.display().to_string()),
        (
            "YGG_KNOWLEDGE_DIR".into(),
            root.join("bundle").display().to_string(),
        ),
        (
            "YGG_KNOWLEDGE_POLICY_DIR".into(),
            root.join("policy").display().to_string(),
        ),
    ]))
    .unwrap();
    let request = CoordinatorBinding {
        migration_operation: Uuid::new_v4(),
        participant: Uuid::new_v4(),
    };
    let selection = root.join("policy/runtime.json");
    let journal = root.join("policy/sql-fence-1.json");
    let identity_path = root.join("policy/identity.json");
    let identity = std::fs::read(&identity_path).unwrap();
    assert!(!selection.exists());
    assert!(prepare_sql(&config, &binding, request).is_err());
    assert!(!selection.exists());
    binding.phase = Phase::Fenced;
    binding.generation = 1;
    let first = serde_json::to_value(prepare_sql(&config, &binding, request).unwrap()).unwrap();
    let intent = std::fs::read(&journal).unwrap();
    let fenced = std::fs::read(&selection).unwrap();
    assert_eq!(first["source_generation"], 1);
    assert_eq!(
        first["coordinator"]["participant"],
        request.participant.to_string()
    );
    assert_eq!(std::fs::read(&identity_path).unwrap(), identity);
    let output = command(root, root)
        .args(["must not reach SQL", "--global"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cutover is fenced"));
    assert!(!root.join("data").exists());
    assert_eq!(
        serde_json::to_value(prepare_sql(&config, &binding, request).unwrap()).unwrap(),
        first
    );
    for other in [
        CoordinatorBinding {
            migration_operation: Uuid::new_v4(),
            ..request
        },
        CoordinatorBinding {
            participant: Uuid::new_v4(),
            ..request
        },
    ] {
        assert!(prepare_sql(&config, &binding, other).is_err());
        assert_eq!(std::fs::read(&journal).unwrap(), intent);
        assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    }
    // Retained intent survives a crash before the first selection publication.
    std::fs::remove_file(&selection).unwrap();
    assert_eq!(
        serde_json::to_value(prepare_sql(&config, &binding, request).unwrap()).unwrap(),
        first
    );
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    assert_eq!(std::fs::read(&journal).unwrap(), intent);
    std::fs::write(&selection, "independently changed selection").unwrap();
    assert!(prepare_sql(&config, &binding, request).is_err());
    assert_eq!(
        std::fs::read_to_string(&selection).unwrap(),
        "independently changed selection"
    );
    std::fs::write(&selection, &fenced).unwrap();
    let edited_identity = format!("{}\n", String::from_utf8(identity.clone()).unwrap());
    std::fs::write(&identity_path, &edited_identity).unwrap();
    assert!(prepare_sql(&config, &binding, request).is_err());
    assert_eq!(
        std::fs::read_to_string(&identity_path).unwrap(),
        edited_identity
    );
    std::fs::write(&identity_path, identity).unwrap();
    std::fs::rename(root.join("bundle"), root.join("original-bundle")).unwrap();
    KnowledgeStore::open(&root.join("bundle"), true).unwrap();
    assert!(prepare_sql(&config, &binding, request).is_err());
    assert_eq!(std::fs::read(&selection).unwrap(), fenced);
    assert_eq!(std::fs::read(&journal).unwrap(), intent);
}
