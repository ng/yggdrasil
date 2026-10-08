#![cfg(any(target_os = "macos", target_os = "linux"))]
use chrono::{TimeZone, Utc};
use uuid::Uuid;
use ygg::knowledge::{
    document::{Source, State},
    identity::{GitIdentity, IdentityRegistry},
    matching::Filters,
    service::{Approver, Creation, KnowledgeService, RuleInput},
    store::{ExpectedRevision, KnowledgeStore},
};

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap()
}
fn setup() -> (tempfile::TempDir, KnowledgeService, Uuid) {
    let temp = tempfile::tempdir().unwrap();
    let registry = IdentityRegistry::open(&temp.path().join("policy"), true).unwrap();
    registry.initialize(true).unwrap();
    let repo = registry
        .bind(&GitIdentity {
            common_dir: temp.path().canonicalize().unwrap().join("repo/.git"),
            origin: None,
        })
        .unwrap();
    let store = KnowledgeStore::open(&temp.path().join("bundle"), true).unwrap();
    let service = KnowledgeService::new(store, registry, "test-user".into()).unwrap();
    (temp, service, repo)
}
fn input(repo: Uuid) -> RuleInput {
    RuleInput {
        repo: Some(repo),
        text: "Preserve λ and trailing newline.\n".into(),
        file_glob: Some("src/*.rs".into()),
        ..RuleInput::default()
    }
}

#[test]
fn proposal_approval_edit_and_rejection_keep_the_gate_closed() {
    let (_temp, service, repo) = setup();
    let proposal = service
        .create_rule(input(repo), Creation::Proposal, now())
        .unwrap();
    assert_eq!(service.pending(Some(repo)).unwrap().documents.len(), 1);
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    assert!(
        service
            .approve(
                proposal.key.id,
                &proposal.revision,
                Approver::Agent(Uuid::new_v4()),
                now()
            )
            .is_err()
    );
    let approved = service
        .approve(
            proposal.key.id,
            &proposal.revision,
            Approver::Human(None),
            now(),
        )
        .unwrap();
    let profile = approved.document.profile().unwrap().unwrap();
    assert_eq!(profile.source, Source::Proposed);
    assert!(profile.approval.unwrap().actor.is_none());
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    assert!(service.reject(approved.key.id, &approved.revision).is_err());
    let mut edited = approved.document.clone();
    edited.body.push_str("Changed instruction.");
    let changed = service
        .edit(approved.key.id, &approved.revision, edited)
        .unwrap();
    assert_eq!(
        changed.document.profile().unwrap().unwrap().state,
        State::Pending
    );
    assert!(
        changed
            .document
            .profile()
            .unwrap()
            .unwrap()
            .approval
            .is_none()
    );
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    assert!(
        service
            .approve(
                changed.key.id,
                &approved.revision,
                Approver::Human(None),
                now()
            )
            .is_err()
    );
    service.reject(changed.key.id, &changed.revision).unwrap();
    assert!(service.get(changed.key.id).unwrap().is_none());
}

#[test]
fn explicitly_authorized_lead_can_approve_but_revocation_takes_effect() {
    let (temp, service, repo) = setup();
    let lead = Uuid::new_v4();
    let registry = IdentityRegistry::open(&temp.path().join("policy"), false).unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.approval_leads.insert(lead);
    registry.replace(&revision, &policy).unwrap();
    let first = service
        .create_rule(input(repo), Creation::Proposal, now())
        .unwrap();
    service
        .approve(first.key.id, &first.revision, Approver::Agent(lead), now())
        .unwrap();
    let second = service
        .create_rule(input(repo), Creation::Proposal, now())
        .unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.approval_leads.clear();
    registry.replace(&revision, &policy).unwrap();
    assert!(
        service
            .approve(
                second.key.id,
                &second.revision,
                Approver::Agent(lead),
                now()
            )
            .is_err()
    );
}

#[test]
fn notes_preserve_scope_recency_limit_exact_text_and_revision_checks() {
    let (_temp, service, repo) = setup();
    let global = service
        .create_note(None, " global\n".into(), None, now())
        .unwrap();
    for i in 1..8 {
        service
            .create_note(
                Some(repo),
                format!("note {i}"),
                None,
                now() + chrono::Duration::seconds(i),
            )
            .unwrap();
    }
    let recent = service.notes(Some(repo), false, 5).unwrap();
    assert_eq!(recent.documents.len(), 5);
    assert_eq!(recent.documents[0].document.body, "note 7");
    let globals = service.notes(None, false, 5).unwrap();
    assert_eq!(globals.documents.len(), 1);
    assert_eq!(globals.documents[0].document.body, " global\n");
    assert_eq!(service.notes(None, true, 50).unwrap().documents.len(), 8);
    assert!(service.delete(global.key.id, "stale").is_err());
    service.delete(global.key.id, &global.revision).unwrap();
    assert!(service.notes(None, false, 5).unwrap().documents.is_empty());
    assert!(
        service
            .create_note(Some(Uuid::new_v4()), "unmapped".into(), None, now())
            .is_err()
    );
}

#[test]
fn display_edits_preserve_evidence_and_cannot_mint_approval() {
    let (_temp, service, repo) = setup();
    let manual = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let old_digest = manual.document.approval_digest().unwrap();
    let mut edited = manual.document.clone();
    edited
        .metadata
        .insert("title".into(), "Useful title".into());
    let edited = service
        .edit(manual.key.id, &manual.revision, edited)
        .unwrap();
    assert_eq!(edited.document.approval_digest().unwrap(), old_digest);
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    let pending = service
        .create_rule(input(repo), Creation::ManualPending, now())
        .unwrap();
    let mut forged = pending.document.clone();
    let mut p = forged.profile().unwrap().unwrap();
    p.state = State::Active;
    p.approval = edited.document.profile().unwrap().unwrap().approval;
    forged.set_profile(&p).unwrap();
    let saved = service
        .edit(pending.key.id, &pending.revision, forged)
        .unwrap();
    assert_eq!(
        saved.document.profile().unwrap().unwrap().state,
        State::Pending
    );
    assert!(
        saved
            .document
            .profile()
            .unwrap()
            .unwrap()
            .approval
            .is_none()
    );
}

#[test]
fn changed_deleted_and_untrusted_selections_are_not_injected() {
    let (temp, service, repo) = setup();
    let selected = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let filters = Filters {
        file: Some("src/a.rs"),
        ..Filters::default()
    };
    assert!(
        service
            .revalidate_rule(&selected, &filters, now())
            .unwrap()
            .is_some()
    );
    let registry = IdentityRegistry::open(&temp.path().join("policy"), false).unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = false;
    registry.replace(&revision, &policy).unwrap();
    assert!(
        service
            .revalidate_rule(&selected, &filters, now())
            .unwrap()
            .is_none()
    );
    assert!(service.rules(&filters, now()).unwrap().documents.is_empty());
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = true;
    registry.replace(&revision, &policy).unwrap();
    let mut edited = selected.document.clone();
    edited.body.push('!');
    let saved = service
        .edit(selected.key.id, &selected.revision, edited)
        .unwrap();
    assert!(
        service
            .revalidate_rule(&selected, &filters, now())
            .unwrap()
            .is_none()
    );
    service.delete(saved.key.id, &saved.revision).unwrap();
    assert!(
        service
            .revalidate_rule(&selected, &filters, now())
            .unwrap()
            .is_none()
    );
}

#[test]
fn corpus_wide_uuid_collisions_and_wrong_user_documents_are_excluded() {
    let (temp, service, repo) = setup();
    let first = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let raw = KnowledgeStore::open(&temp.path().join("bundle"), false).unwrap();
    let mut collision = first.document.clone();
    let mut p = collision.profile().unwrap().unwrap();
    p.scope = ygg::knowledge::document::Scope::Global;
    p.repo = None;
    collision.set_profile(&p).unwrap();
    assert!(raw.put(&collision, ExpectedRevision::Absent).is_err());
    let mut other_user = first.document.clone();
    let mut p = other_user.profile().unwrap().unwrap();
    p.id = Uuid::new_v4();
    p.user_id = Some("someone-else".into());
    other_user.set_profile(&p).unwrap();
    raw.put(&other_user, ExpectedRevision::Absent).unwrap();
    assert!(service.get(p.id).is_err());
    assert_eq!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .len(),
        1
    );
    // An unrelated malformed filename must not erase/revalidation-block good rules.
    std::fs::write(
        temp.path()
            .join("bundle")
            .join(first.key.relative_path())
            .with_file_name("not-a-uuid.md"),
        "broken",
    )
    .unwrap();
    let found = service.rules(&Filters::default(), now()).unwrap();
    assert_eq!(found.documents.len(), 1);
    assert!(!found.diagnostics.is_empty());
    assert!(
        service
            .revalidate_rule(&first, &Filters::default(), now())
            .unwrap()
            .is_some()
    );
}

#[test]
fn prime_caps_after_expiry_filtering_and_revalidates_notes() {
    let (temp, service, repo) = setup();
    for i in 1..8 {
        service
            .create_note(
                Some(repo),
                format!("note {i}"),
                None,
                now() + chrono::Duration::seconds(i),
            )
            .unwrap();
    }
    let first = service
        .notes(Some(repo), false, 1)
        .unwrap()
        .documents
        .remove(0);
    let mut expired = first.document.clone();
    expired
        .metadata
        .insert("stale_after".into(), "2026-10-08T00:00:00Z".into());
    service
        .edit(first.key.id, &first.revision, expired)
        .unwrap();
    assert_eq!(
        service
            .notes(Some(repo), false, 50)
            .unwrap()
            .documents
            .len(),
        7
    );
    let prime = service.prime_notes(Some(repo), now()).unwrap();
    assert_eq!(prime.documents.len(), 5);
    assert_eq!(prime.documents[0].document.body, "note 6");
    assert!(
        service
            .revalidate_note(&first, Some(repo), now())
            .unwrap()
            .is_none()
    );
    let selected = &prime.documents[0];
    assert!(
        service
            .revalidate_note(selected, Some(repo), now())
            .unwrap()
            .is_some()
    );
    let registry = IdentityRegistry::open(&temp.path().join("policy"), false).unwrap();
    let (mut policy, revision) = registry.read().unwrap();
    policy.trusted = false;
    registry.replace(&revision, &policy).unwrap();
    assert!(
        service
            .revalidate_note(selected, Some(repo), now())
            .unwrap()
            .is_none()
    );
    assert!(
        service
            .prime_notes(Some(repo), now())
            .unwrap()
            .documents
            .is_empty()
    );
}

#[test]
fn externally_duplicated_ids_never_activate_either_copy() {
    let (temp, service, repo) = setup();
    let original = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let untouched = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let target = temp.path().join("bundle/global/learnings");
    std::fs::create_dir_all(&target).unwrap();
    let mut copy = original.document.clone();
    let mut p = copy.profile().unwrap().unwrap();
    p.repo = None;
    p.scope = ygg::knowledge::document::Scope::Global;
    copy.set_profile(&p).unwrap();
    std::fs::write(
        target.join(format!("{}.md", original.key.id)),
        copy.serialize().unwrap(),
    )
    .unwrap();
    assert!(service.get(original.key.id).is_err());
    let rules = service.rules(&Filters::default(), now()).unwrap();
    assert_eq!(rules.documents.len(), 1);
    assert_eq!(rules.documents[0].key.id, untouched.key.id);
    assert_eq!(rules.diagnostics.len(), 2);
    assert!(
        service
            .revalidate_rule(&original, &Filters::default(), now())
            .is_err()
    );
}

#[test]
fn external_content_edits_need_review_but_expiry_does_not_invent_a_proposal() {
    let (temp, service, repo) = setup();
    let first = service
        .create_rule(input(repo), Creation::ManualActive, now())
        .unwrap();
    let path = temp.path().join("bundle").join(first.key.relative_path());
    let mut external = first.document.clone();
    external.body.push_str("External unreviewed instruction.");
    std::fs::write(&path, external.serialize().unwrap()).unwrap();
    let pending = service.pending(Some(repo)).unwrap();
    assert_eq!(pending.documents.len(), 1);
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    let approved = service
        .approve(
            first.key.id,
            &pending.documents[0].revision,
            Approver::Human(None),
            now(),
        )
        .unwrap();
    let mut expired = approved.document.clone();
    expired
        .metadata
        .insert("stale_after".into(), "2026-10-07T00:00:00Z".into());
    let expired = service
        .edit(first.key.id, &approved.revision, expired)
        .unwrap();
    assert!(service.pending(Some(repo)).unwrap().documents.is_empty());
    assert!(
        service
            .rules(&Filters::default(), now())
            .unwrap()
            .documents
            .is_empty()
    );
    assert!(
        service
            .approve(
                first.key.id,
                &expired.revision,
                Approver::Human(None),
                now()
            )
            .is_err()
    );
}
