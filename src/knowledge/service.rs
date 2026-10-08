//! Database-independent note/rule operations. The command adapter supplies the
//! authenticated caller context; this does not defend against the OS owner
//! directly rewriting their own trusted corpus or policy configuration.
use super::{
    document::{ActivationKind, Document, Profile, Scope, Source, State},
    identity::{Identities, IdentityRegistry},
    matching::{self, Filters},
    store::{ExpectedRevision, Key, Kind, KnowledgeStore, RevisionedDocument, Snapshot},
};
use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use uuid::Uuid;

pub enum Approver {
    Human(Option<Uuid>),
    Agent(Uuid),
}
pub enum Creation {
    ManualActive,
    ManualPending,
    Proposal,
}

#[derive(Default)]
pub struct RuleInput {
    pub repo: Option<Uuid>,
    pub text: String,
    pub context: Option<String>,
    pub file_glob: Option<String>,
    pub rule_id: Option<String>,
    pub scope_tags: BTreeMap<String, serde_json::Value>,
    pub created_by: Option<Uuid>,
}

pub struct KnowledgeService {
    store: KnowledgeStore,
    registry: IdentityRegistry,
    user: String,
}

impl KnowledgeService {
    pub fn new(store: KnowledgeStore, registry: IdentityRegistry, user: String) -> Result<Self> {
        ensure!(
            !user.trim().is_empty(),
            "explicit nonempty knowledge user mapping required"
        );
        registry.read()?;
        Ok(Self {
            store,
            registry,
            user,
        })
    }

    fn policy(&self) -> Result<Identities> {
        Ok(self.registry.read()?.0)
    }
    fn scope(policy: &Identities, repo: Option<Uuid>) -> Result<()> {
        ensure!(
            repo.is_none() || policy.repos.iter().any(|r| Some(r.id) == repo),
            "unmapped portable repository scope"
        );
        Ok(())
    }
    fn owned(&self, doc: &Document) -> Result<Profile> {
        let profile = doc
            .profile()?
            .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
        ensure!(
            profile.user_id.as_deref() == Some(&self.user),
            "knowledge belongs to an unmapped or different user"
        );
        Self::scope(&self.policy()?, profile.repo)?;
        Ok(profile)
    }
    pub fn get(&self, id: Uuid) -> Result<Option<RevisionedDocument>> {
        let document = self.store.find(id)?;
        if let Some(doc) = &document {
            self.owned(&doc.document)?;
        }
        Ok(document)
    }
    fn expected(&self, id: Uuid, revision: &str) -> Result<RevisionedDocument> {
        let doc = self
            .get(id)?
            .ok_or_else(|| anyhow!("knowledge document not found"))?;
        ensure!(
            doc.revision == revision,
            "knowledge revision conflict; reload before editing"
        );
        Ok(doc)
    }

    fn create(
        &self,
        input: RuleInput,
        kind: Kind,
        creation: Creation,
        now: DateTime<Utc>,
    ) -> Result<RevisionedDocument> {
        ensure!(
            !input.text.trim().is_empty(),
            "knowledge text cannot be empty"
        );
        let policy = self.policy()?;
        Self::scope(&policy, input.repo)?;
        let profile = Profile {
            schema_version: 1,
            id: Uuid::new_v4(),
            scope: if input.repo.is_some() {
                Scope::Repo
            } else {
                Scope::Global
            },
            repo: input.repo,
            legacy_repo_id: None,
            user_id: Some(self.user.clone()),
            created_by: input.created_by,
            created_at: now,
            context: input.context,
            file_glob: input.file_glob,
            rule_id: input.rule_id,
            scope_tags: input.scope_tags,
            state: State::Pending,
            source: if matches!(creation, Creation::Proposal) {
                Source::Proposed
            } else {
                Source::Manual
            },
            approval: None,
            extra: BTreeMap::new(),
        };
        let mut document = Document {
            metadata: serde_yaml_ng::Mapping::new(),
            body: input.text,
        };
        document.metadata.insert(
            "type".into(),
            match kind {
                Kind::Note => "Note",
                Kind::Learning => "Engineering Rule",
            }
            .into(),
        );
        document.set_profile(&profile)?;
        if kind == Kind::Note {
            let mut p = profile;
            p.state = State::Active;
            document.set_profile(&p)?;
        } else if matches!(creation, Creation::ManualActive) {
            document.activate(
                policy.corpus_id,
                ActivationKind::Manual,
                profile.created_by,
                Some(now),
            )?;
        }
        self.store.put(&document, ExpectedRevision::Absent)
    }

    pub fn create_note(
        &self,
        repo: Option<Uuid>,
        text: String,
        created_by: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<RevisionedDocument> {
        self.create(
            RuleInput {
                repo,
                text,
                created_by,
                ..RuleInput::default()
            },
            Kind::Note,
            Creation::ManualActive,
            now,
        )
    }
    pub fn create_rule(
        &self,
        input: RuleInput,
        creation: Creation,
        now: DateTime<Utc>,
    ) -> Result<RevisionedDocument> {
        self.create(input, Kind::Learning, creation, now)
    }

    pub fn approve(
        &self,
        id: Uuid,
        revision: &str,
        approver: Approver,
        now: DateTime<Utc>,
    ) -> Result<RevisionedDocument> {
        let policy = self.policy()?;
        let actor = match approver {
            Approver::Human(id) => id,
            Approver::Agent(id) => {
                ensure!(
                    policy.approval_leads.contains(&id),
                    "agent is not an explicitly authorized approval lead"
                );
                Some(id)
            }
        };
        let old = self.expected(id, revision)?;
        ensure!(
            old.key.kind == Kind::Learning,
            "only a learning can be approved"
        );
        let profile = self.owned(&old.document)?;
        ensure!(
            profile.state == State::Pending || !old.document.activation_valid(policy.corpus_id)?,
            "learning is already active"
        );
        let mut document = old.document;
        document.activate(policy.corpus_id, ActivationKind::Reviewed, actor, Some(now))?;
        self.store
            .put(&document, ExpectedRevision::Digest(revision))
    }

    pub fn reject(&self, id: Uuid, revision: &str) -> Result<()> {
        let old = self.expected(id, revision)?;
        ensure!(
            old.key.kind == Kind::Learning,
            "only a learning can be rejected"
        );
        self.owned(&old.document)?;
        ensure!(
            !old.document.activation_valid(self.policy()?.corpus_id)?,
            "only pending proposals can be rejected"
        );
        self.store.delete(old.key, revision)
    }

    pub fn delete(&self, id: Uuid, revision: &str) -> Result<()> {
        let old = self.expected(id, revision)?;
        self.store.delete(old.key, revision)
    }

    /// Covered edits become pending. Display-only changes retain prior evidence.
    /// This API cannot mint approval or alter original provenance by editing YAML.
    pub fn edit(
        &self,
        id: Uuid,
        revision: &str,
        mut edited: Document,
    ) -> Result<RevisionedDocument> {
        let old = self.expected(id, revision)?;
        let old_profile = self.owned(&old.document)?;
        let mut profile = self.owned(&edited)?;
        ensure!(
            Key::from_document(&edited)? == old.key,
            "identity/scope moves require an explicit move operation"
        );
        ensure!(
            profile.created_by == old_profile.created_by
                && profile.created_at == old_profile.created_at
                && profile.source == old_profile.source
                && profile.legacy_repo_id == old_profile.legacy_repo_id,
            "edit cannot change original provenance"
        );
        ensure!(
            !edited.body.trim().is_empty(),
            "knowledge text cannot be empty"
        );
        profile.state = old_profile.state;
        profile.approval = old_profile.approval;
        if old.key.kind == Kind::Learning
            && edited.approval_digest()? != old.document.approval_digest()?
        {
            profile.state = State::Pending;
            profile.approval = None;
        }
        edited.set_profile(&profile)?;
        self.store.put(&edited, ExpectedRevision::Digest(revision))
    }

    /// Moving a rule always requires review in its new matching scope. Original
    /// creator/time and legacy provenance remain intact; UUID never changes.
    pub fn move_scope(
        &self,
        id: Uuid,
        revision: &str,
        repo: Option<Uuid>,
    ) -> Result<RevisionedDocument> {
        Self::scope(&self.policy()?, repo)?;
        let old = self.expected(id, revision)?;
        ensure!(old.key.repo != repo, "document already has requested scope");
        let mut document = old.document;
        let mut profile = self.owned(&document)?;
        profile.repo = repo;
        profile.scope = if repo.is_some() {
            Scope::Repo
        } else {
            Scope::Global
        };
        if old.key.kind == Kind::Learning {
            profile.state = State::Pending;
            profile.approval = None;
        }
        document.set_profile(&profile)?;
        self.store.move_document(old.key, &document, revision)
    }

    fn browse(&self) -> Result<Snapshot> {
        let policy = self.policy()?;
        let mut snapshot = self.store.snapshot();
        snapshot
            .documents
            .retain(|doc| match doc.document.profile() {
                Ok(Some(p)) if p.user_id.as_deref() == Some(&self.user) => {
                    if Self::scope(&policy, p.repo).is_ok() {
                        true
                    } else {
                        snapshot
                            .diagnostics
                            .push(format!("{}: unmapped repository scope", doc.key.id));
                        false
                    }
                }
                Ok(_) => false,
                Err(e) => {
                    snapshot.diagnostics.push(format!("{}: {e}", doc.key.id));
                    false
                }
            });
        Ok(snapshot)
    }

    pub fn notes(&self, repo: Option<Uuid>, all: bool, limit: usize) -> Result<Snapshot> {
        Self::scope(&self.policy()?, repo)?;
        let mut snapshot = self.browse()?;
        snapshot
            .documents
            .retain(|d| d.key.kind == Kind::Note && matching::note_scope(d.key.repo, repo, all));
        snapshot.documents.sort_by_cached_key(|doc| {
            let profile = doc.document.profile().unwrap().unwrap();
            (std::cmp::Reverse(profile.created_at), profile.id)
        });
        snapshot.documents.truncate(limit);
        Ok(snapshot)
    }

    /// Prime keeps the established five-note cap, after freshness filtering.
    /// Explicit browsing remains able to display stale/deprecated documents.
    pub fn prime_notes(&self, repo: Option<Uuid>, now: DateTime<Utc>) -> Result<Snapshot> {
        let mut snapshot = self.notes(repo, false, usize::MAX)?;
        if !self.policy()?.trusted {
            snapshot.documents.clear();
            return Ok(snapshot);
        }
        snapshot
            .documents
            .retain(|doc| match doc.document.current(now) {
                Ok(current) => {
                    current && doc.document.profile().unwrap().unwrap().state == State::Active
                }
                Err(e) => {
                    snapshot.diagnostics.push(format!("{}: {e}", doc.key.id));
                    false
                }
            });
        snapshot.documents.truncate(5);
        Ok(snapshot)
    }

    pub fn revalidate_note(
        &self,
        selected: &RevisionedDocument,
        repo: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<Option<Document>> {
        let policy = self.policy()?;
        Self::scope(&policy, repo)?;
        if !policy.trusted {
            return Ok(None);
        }
        let Some(current) = self.get(selected.key.id)? else {
            return Ok(None);
        };
        if current.key.kind != Kind::Note
            || current.revision != selected.revision
            || !matching::note_scope(current.key.repo, repo, false)
            || !current.document.current(now)?
            || self.owned(&current.document)?.state != State::Active
        {
            return Ok(None);
        }
        Ok(Some(current.document))
    }

    pub fn pending(&self, repo: Option<Uuid>) -> Result<Snapshot> {
        let policy = self.policy()?;
        Self::scope(&policy, repo)?;
        let mut snapshot = self.browse()?;
        snapshot.documents.retain(|d| {
            d.key.kind == Kind::Learning
                && (repo.is_none() || d.key.repo.is_none() || repo == d.key.repo)
                && !d
                    .document
                    .activation_valid(policy.corpus_id)
                    .unwrap_or(false)
        });
        snapshot.documents.sort_by_cached_key(|doc| {
            let profile = doc.document.profile().unwrap().unwrap();
            (std::cmp::Reverse(profile.created_at), profile.id)
        });
        Ok(snapshot)
    }

    pub fn rules(&self, filters: &Filters<'_>, now: DateTime<Utc>) -> Result<Snapshot> {
        let policy = self.policy()?;
        Self::scope(&policy, filters.repo)?;
        let mut snapshot = self.browse()?;
        let trusted = policy.trusted.then_some(policy.corpus_id);
        snapshot.documents.retain(|doc| {
            let result = (|| -> Result<bool> {
                Ok(doc.key.kind == Kind::Learning
                    && doc.document.eligible(trusted, now)?
                    && matching::matches(&doc.document.profile()?.unwrap(), filters)?)
            })();
            match result {
                Ok(eligible) => eligible,
                Err(e) => {
                    snapshot.diagnostics.push(format!("{}: {e}", doc.key.id));
                    false
                }
            }
        });
        snapshot.documents.sort_by(|a, b| {
            matching::compare(
                &a.document.profile().unwrap().unwrap(),
                &b.document.profile().unwrap().unwrap(),
            )
        });
        Ok(snapshot)
    }

    /// Use one current corpus inventory for an injection batch; every retained
    /// document still has fresh bytes, policy, ownership and eligibility checks.
    pub fn revalidate_rules(
        &self,
        selected: &[RevisionedDocument],
        filters: &Filters<'_>,
        now: DateTime<Utc>,
    ) -> Result<Snapshot> {
        let policy = self.policy()?;
        Self::scope(&policy, filters.repo)?;
        let mut snapshot = self.store.revalidate_selected(selected);
        snapshot.documents.retain(|doc| {
            let result = (|| -> Result<bool> {
                Ok(doc.key.kind == Kind::Learning
                    && doc
                        .document
                        .eligible(policy.trusted.then_some(policy.corpus_id), now)?
                    && matching::matches(&self.owned(&doc.document)?, filters)?)
            })();
            match result {
                Ok(eligible) => eligible,
                Err(e) => {
                    snapshot.diagnostics.push(format!("{}: {e}", doc.key.id));
                    false
                }
            }
        });
        Ok(snapshot)
    }

    pub fn revalidate_notes(
        &self,
        selected: &[RevisionedDocument],
        repo: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> Result<Snapshot> {
        let policy = self.policy()?;
        Self::scope(&policy, repo)?;
        let mut snapshot = self.store.revalidate_selected(selected);
        snapshot.documents.retain(|doc| {
            let result = (|| -> Result<bool> {
                Ok(policy.trusted
                    && doc.key.kind == Kind::Note
                    && matching::note_scope(doc.key.repo, repo, false)
                    && doc.document.current(now)?
                    && self.owned(&doc.document)?.state == State::Active)
            })();
            match result {
                Ok(eligible) => eligible,
                Err(e) => {
                    snapshot.diagnostics.push(format!("{}: {e}", doc.key.id));
                    false
                }
            }
        });
        Ok(snapshot)
    }

    /// Revalidate selected bytes, corpus trust and eligibility immediately before
    /// injection. A changed/deleted/revoked selection must be omitted, not used
    /// from an earlier snapshot or disposable index.
    pub fn revalidate_rule(
        &self,
        selected: &RevisionedDocument,
        filters: &Filters<'_>,
        now: DateTime<Utc>,
    ) -> Result<Option<Document>> {
        let policy = self.policy()?;
        let Some(current) = self.get(selected.key.id)? else {
            return Ok(None);
        };
        if current.revision != selected.revision
            || !current
                .document
                .eligible(policy.trusted.then_some(policy.corpus_id), now)?
            || !matching::matches(&self.owned(&current.document)?, filters)?
        {
            return Ok(None);
        }
        Ok(Some(current.document))
    }
}
