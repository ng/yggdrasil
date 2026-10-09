//! Pure legacy-model adapters. Callers must supply explicit deployment/owner
//! mappings and usage records; these functions neither connect nor cut over.
use super::document::{ActivationKind, Document, Profile, Scope, Source, State};
use crate::models::{learning::Learning, memory::Memory};
use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Explicit source IDs -> portable identities. Empty legacy users require an
/// explicit entry just like named users; there is no current-user fallback.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mappings {
    pub database_id: Uuid,
    pub corpus_id: Uuid,
    #[serde(deserialize_with = "unique_map")]
    pub repos: BTreeMap<Uuid, Uuid>,
    #[serde(deserialize_with = "unique_map")]
    pub users: BTreeMap<String, String>,
}

fn unique_map<'de, D, K, V>(deserializer: D) -> std::result::Result<BTreeMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct Unique<K, V>(std::marker::PhantomData<(K, V)>);
    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> serde::de::Visitor<'de> for Unique<K, V> {
        type Value = BTreeMap<K, V>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("identity map with unique source keys")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut source: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = source.next_entry()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate identity mapping"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub corpus_id: Uuid,
    pub document_id: Uuid,
    pub applied_count: i32,
    pub last_applied_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize)]
struct Provenance {
    database_id: Uuid,
    user_id: String,
    // Pending rows can contain historical approval fields; preserve these
    // without converting them into activation evidence.
    approved_at: Option<DateTime<Utc>>,
    approved_by: Option<Uuid>,
    // SQL allows arbitrary JSONB. Non-object values have no matching keys but
    // must still survive a legacy round trip (including JSON null).
    non_object_scope_tags: Option<RawTags>,
}
#[derive(Serialize, Deserialize)]
struct RawTags {
    value: serde_json::Value,
}
const PROVENANCE: &str = "legacy";

impl Mappings {
    fn profile(
        &self,
        id: Uuid,
        repo: Option<Uuid>,
        user: &str,
        created_by: Option<Uuid>,
        created_at: DateTime<Utc>,
    ) -> Result<Profile> {
        let owner = self
            .users
            .get(user)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("explicit nonempty user mapping required for {user:?}"))?;
        let portable = repo
            .map(|repo| {
                self.repos
                    .get(&repo)
                    .copied()
                    .ok_or_else(|| anyhow!("unmapped legacy repository {repo}"))
            })
            .transpose()?;
        Ok(Profile {
            schema_version: 1,
            id,
            scope: if portable.is_some() {
                Scope::Repo
            } else {
                Scope::Global
            },
            repo: portable,
            legacy_repo_id: repo,
            user_id: Some(owner.clone()),
            created_by,
            created_at,
            context: None,
            file_glob: None,
            rule_id: None,
            scope_tags: BTreeMap::new(),
            state: State::Pending,
            source: Source::Manual,
            approval: None,
            extra: BTreeMap::new(),
        })
    }

    fn repo(&self, p: &Profile) -> Result<Option<Uuid>> {
        let Some(portable) = p.repo else {
            return Ok(None);
        };
        let provenance = provenance(p)?;
        // Preserve duplicate legacy IDs on same-database round trips, but never
        // reuse the original scope after an explicit document scope move.
        if provenance
            .as_ref()
            .is_some_and(|p| p.database_id == self.database_id)
        {
            if let Some(legacy) = p
                .legacy_repo_id
                .filter(|legacy| self.repos.get(legacy) == Some(&portable))
            {
                return Ok(Some(legacy));
            }
        }
        let candidates: Vec<_> = self
            .repos
            .iter()
            .filter(|(_, repo)| **repo == portable)
            .map(|(legacy, _)| *legacy)
            .collect();
        ensure!(
            candidates.len() == 1,
            "portable repository needs one explicit legacy mapping for this output"
        );
        Ok(Some(candidates[0]))
    }
}

fn provenance(p: &Profile) -> Result<Option<Provenance>> {
    p.extra
        .get(PROVENANCE)
        .map(|v| serde_yaml_ng::from_value(v.clone()).map_err(Into::into))
        .transpose()
}
fn document(kind: &str, text: &str, profile: &Profile) -> Result<Document> {
    let mut doc = Document {
        metadata: serde_yaml_ng::Mapping::new(),
        body: text.to_owned(),
    };
    doc.metadata.insert("type".into(), kind.into());
    doc.set_profile(profile)?;
    // Import must obey the same bounded representation as the durable store.
    Document::parse(&doc.serialize()?)
}

pub fn import_note(row: &Memory, legacy_user: &str, mappings: &Mappings) -> Result<Document> {
    let mut p = mappings.profile(
        row.memory_id,
        row.repo_id,
        legacy_user,
        row.created_by,
        row.created_at,
    )?;
    p.state = State::Active;
    p.extra.insert(
        PROVENANCE.into(),
        serde_yaml_ng::to_value(Provenance {
            database_id: mappings.database_id,
            user_id: legacy_user.into(),
            approved_at: None,
            approved_by: None,
            non_object_scope_tags: None,
        })?,
    );
    document("Note", &row.text, &p)
}

/// Only the guarded migration of the selected source database may preserve its
/// activation this way. Ordinary bundle import must reset activation instead.
pub fn import_learning(
    row: &Learning,
    legacy_user: &str,
    mappings: &Mappings,
) -> Result<(Document, Usage)> {
    let mut p = mappings.profile(
        row.learning_id,
        row.repo_id,
        legacy_user,
        row.created_by,
        row.created_at,
    )?;
    p.state = match row.status.as_str() {
        "active" => State::Active,
        "pending" => State::Pending,
        _ => return Err(anyhow!("unrepresentable legacy status {:?}", row.status)),
    };
    p.source = match row.source.as_str() {
        "manual" => Source::Manual,
        "proposed" => Source::Proposed,
        _ => return Err(anyhow!("unrepresentable legacy source {:?}", row.source)),
    };
    p.file_glob = row.file_glob.clone();
    p.rule_id = row.rule_id.clone();
    p.context = row.context.clone();
    let raw_tags = match &row.scope_tags {
        serde_json::Value::Object(tags) => {
            p.scope_tags = tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            None
        }
        other => Some(RawTags {
            value: other.clone(),
        }),
    };
    p.extra.insert(
        PROVENANCE.into(),
        serde_yaml_ng::to_value(Provenance {
            database_id: mappings.database_id,
            user_id: legacy_user.into(),
            approved_at: (p.state == State::Pending)
                .then_some(row.approved_at)
                .flatten(),
            approved_by: (p.state == State::Pending)
                .then_some(row.approved_by)
                .flatten(),
            non_object_scope_tags: raw_tags,
        })?,
    );
    let mut doc = document("Engineering Rule", &row.text, &p)?;
    if p.state == State::Active {
        doc.activate(
            mappings.corpus_id,
            ActivationKind::Legacy,
            row.approved_by,
            row.approved_at,
        )?;
    }
    Document::parse(&doc.serialize()?)?;
    Ok((
        doc,
        Usage {
            corpus_id: mappings.corpus_id,
            document_id: row.learning_id,
            applied_count: row.applied_count,
            last_applied_at: row.last_applied_at,
        },
    ))
}

pub fn note_json_model(doc: &Document, mappings: &Mappings) -> Result<Memory> {
    ensure!(doc.document_type()? == "Note", "note document required");
    let p = doc
        .profile()?
        .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
    Ok(Memory {
        memory_id: p.id,
        repo_id: mappings.repo(&p)?,
        text: doc.body.clone(),
        created_by: p.created_by,
        created_at: p.created_at,
    })
}

pub fn learning_json_model(doc: &Document, usage: &Usage, mappings: &Mappings) -> Result<Learning> {
    ensure!(
        doc.document_type()? == "Engineering Rule",
        "learning document required"
    );
    let p = doc
        .profile()?
        .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
    ensure!(
        usage.corpus_id == mappings.corpus_id && usage.document_id == p.id,
        "usage identity does not match document/corpus"
    );
    let provenance = provenance(&p)?;
    let active = doc.activation_valid(mappings.corpus_id)?;
    let (approved_at, approved_by) = if active {
        let approval = p.approval.as_ref().unwrap();
        if approval.kind == ActivationKind::Manual {
            // Manual creation has activation evidence, but was not a separate
            // approval action in the legacy API.
            (None, None)
        } else {
            (approval.at, approval.actor)
        }
    } else if p.state == State::Pending && p.approval.is_none() {
        provenance
            .as_ref()
            .map(|p| (p.approved_at, p.approved_by))
            .unwrap_or_default()
    } else {
        (None, None)
    };
    let tags = match provenance.and_then(|p| p.non_object_scope_tags) {
        Some(raw) => {
            ensure!(
                !raw.value.is_object(),
                "legacy raw tags must remain non-object"
            );
            ensure!(
                p.scope_tags.is_empty(),
                "legacy non-object tags conflict with edited matching tags"
            );
            raw.value
        }
        None => serde_json::to_value(&p.scope_tags)?,
    };
    Ok(Learning {
        learning_id: p.id,
        repo_id: mappings.repo(&p)?,
        file_glob: p.file_glob,
        rule_id: p.rule_id,
        text: doc.body.clone(),
        context: p.context,
        created_by: p.created_by,
        created_at: p.created_at,
        applied_count: usage.applied_count,
        last_applied_at: usage.last_applied_at,
        scope_tags: tags,
        status: if active { "active" } else { "pending" }.into(),
        source: match p.source {
            Source::Manual => "manual",
            Source::Proposed => "proposed",
        }
        .into(),
        approved_at,
        approved_by,
    })
}

/// Owner field omitted by legacy JSON models but required for lossless SQL
/// export. Rebinding to another database requires explicit inverse mappings.
pub fn legacy_user_id(doc: &Document, mappings: &Mappings) -> Result<String> {
    let p = doc
        .profile()?
        .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
    let owner = p
        .user_id
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("document has no mapped owner"))?;
    if let Some(original) = provenance(&p)? {
        if original.database_id == mappings.database_id
            && mappings.users.get(&original.user_id) == Some(owner)
        {
            return Ok(original.user_id);
        }
    }
    let candidates: Vec<_> = mappings
        .users
        .iter()
        .filter(|(_, current)| *current == owner)
        .map(|(legacy, _)| legacy.clone())
        .collect();
    ensure!(
        candidates.len() == 1,
        "owner needs one explicit legacy mapping for this output"
    );
    Ok(candidates[0].clone())
}

/// Migrated rules require recorded usage; absence must not fabricate zero totals.
pub fn is_imported(doc: &Document) -> Result<bool> {
    Ok(doc
        .profile()?
        .map(|p| provenance(&p))
        .transpose()?
        .flatten()
        .is_some())
}

// These are representation fields introduced by the OKF transport, not SQL
// knowledge fields. Validate their meaning before normalizing them for comparison.
fn reverse_comparable(
    doc: &Document,
    mappings: &Mappings,
) -> Result<(Document, serde_json::Value)> {
    let mut normalized = doc.clone();
    let mut p = doc
        .profile()?
        .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
    let mut legacy_fields = serde_json::json!({"tags": null, "at": null, "actor": null});
    if let Some(raw) = p.extra.get(PROVENANCE) {
        let original: Provenance = serde_yaml_ng::from_value(raw.clone())?;
        ensure!(
            original.database_id == mappings.database_id
                && serde_yaml_ng::to_value(&original)? == *raw,
            "legacy provenance has foreign identity or unrepresentable fields"
        );
        legacy_fields["tags"] = serde_json::to_value(original.non_object_scope_tags)?;
        if p.state == State::Pending {
            legacy_fields["at"] = serde_json::to_value(original.approved_at)?;
            legacy_fields["actor"] = serde_json::to_value(original.approved_by)?;
        }
    }
    // This is the original scope hint, intentionally retained after a scope
    // move. The adapter already resolved the CURRENT scope through explicit
    // mappings, and the reconstructed profile must have that same scope.
    p.legacy_repo_id = None;
    p.extra.remove(PROVENANCE);
    if let Some(approval) = &mut p.approval {
        // SQL stores activation plus actor/time; corpus/digest are regenerated
        // from the same identity/content. Keep all other approval fields exact.
        approval.kind = ActivationKind::Legacy;
    }
    normalized.set_profile(&p)?;
    Ok((normalized, legacy_fields))
}

/// Strict rollback conversion, unlike the display-only JSON adapter. Any field
/// outside the legacy representation blocks rollback instead of disappearing.
pub fn reverse_note(doc: &Document, mappings: &Mappings) -> Result<(Memory, String)> {
    let user = legacy_user_id(doc, mappings)?;
    let row = note_json_model(doc, mappings)?;
    let restored = import_note(&row, &user, mappings)?;
    ensure!(
        reverse_comparable(doc, mappings)? == reverse_comparable(&restored, mappings)?,
        "note cannot round-trip losslessly through legacy SQL"
    );
    Ok((row, user))
}

pub fn reverse_learning(
    doc: &Document,
    usage: &Usage,
    mappings: &Mappings,
) -> Result<(Learning, String)> {
    let user = legacy_user_id(doc, mappings)?;
    let p = doc
        .profile()?
        .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
    let mut row = learning_json_model(doc, usage, mappings)?;
    if p.state == State::Active {
        ensure!(
            doc.activation_valid(mappings.corpus_id)?,
            "active rule has invalid approval; review before rollback"
        );
        let approval = p.approval.as_ref().unwrap();
        // The public JSON adapter omits manual creation evidence for API
        // compatibility. Reverse import must preserve its real actor and time.
        row.approved_at = approval.at;
        row.approved_by = approval.actor;
    }
    let (restored, restored_usage) = import_learning(&row, &user, mappings)?;
    ensure!(
        restored_usage == *usage
            && reverse_comparable(doc, mappings)? == reverse_comparable(&restored, mappings)?,
        "rule cannot round-trip losslessly through legacy SQL"
    );
    Ok((row, user))
}
