//! OKF 0.2 document model and Yggdrasil activation evidence.
//!
//! Specification revision and legacy mappings live in
//! tests/fixtures/knowledge/CONTRACT.md. Unknown YAML fields survive serialization;
//! the Markdown body is never trimmed, reformatted, interpreted, or executed.
use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub const PARSER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub metadata: Mapping,
    pub body: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Global,
    Repo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Pending,
    Active,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Manual,
    Proposed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ActivationKind {
    Manual,
    Reviewed,
    Legacy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub kind: ActivationKind,
    pub corpus_id: Uuid,
    pub digest: String,
    // Legacy records can have neither actor nor time. Do not invent evidence.
    pub actor: Option<Uuid>,
    pub at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub schema_version: u32,
    pub id: Uuid,
    pub scope: Scope,
    pub repo: Option<Uuid>,
    pub legacy_repo_id: Option<Uuid>,
    pub user_id: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub context: Option<String>,
    pub file_glob: Option<String>,
    pub rule_id: Option<String>,
    #[serde(default)]
    pub scope_tags: BTreeMap<String, serde_json::Value>,
    pub state: State,
    pub source: Source,
    pub approval: Option<Approval>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Document {
    pub fn parse(text: &str) -> Result<Self> {
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "document exceeds byte limit"
        );
        let mut lines = text.split_inclusive('\n');
        ensure!(
            lines
                .next()
                .is_some_and(|line| line.trim_end_matches(['\r', '\n']) == "---"),
            "missing OKF frontmatter"
        );
        let start = text
            .find('\n')
            .ok_or_else(|| anyhow!("unclosed OKF frontmatter"))?
            + 1;
        let mut offset = start;
        for line in lines {
            if line.trim_end_matches(['\r', '\n']) == "---" {
                ensure!(
                    offset - start <= MAX_FRONTMATTER_BYTES,
                    "frontmatter exceeds byte limit"
                );
                let metadata: Mapping = super::yaml::parse(&text[start..offset])?;
                let document = Self {
                    metadata,
                    body: text[offset + line.len()..].into(),
                };
                document.document_type()?;
                return Ok(document);
            }
            offset += line.len();
            ensure!(
                offset - start <= MAX_FRONTMATTER_BYTES,
                "frontmatter exceeds byte limit"
            );
        }
        bail!("unclosed OKF frontmatter")
    }

    pub fn document_type(&self) -> Result<&str> {
        self.metadata
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("OKF type must be a nonempty string"))
    }

    pub fn serialize(&self) -> Result<String> {
        self.document_type()?;
        let frontmatter = serde_yaml_ng::to_string(&self.metadata)?;
        ensure!(
            frontmatter.len() <= MAX_FRONTMATTER_BYTES,
            "frontmatter exceeds byte limit"
        );
        let text = format!("---\n{frontmatter}---\n{}", self.body);
        ensure!(
            text.len() <= MAX_DOCUMENT_BYTES,
            "document exceeds byte limit"
        );
        Ok(text)
    }

    /// Generic OKF remains browseable without a Yggdrasil profile.
    pub fn profile(&self) -> Result<Option<Profile>> {
        let Some(value) = self.metadata.get("ygg") else {
            return Ok(None);
        };
        let profile: Profile = serde_yaml_ng::from_value(value.clone())?;
        ensure!(
            profile.schema_version == 1,
            "unsupported Yggdrasil profile version"
        );
        ensure!(
            matches!(
                (profile.scope, profile.repo),
                (Scope::Global, None) | (Scope::Repo, Some(_))
            ),
            "ambiguous knowledge scope"
        );
        Ok(Some(profile))
    }

    pub fn set_profile(&mut self, profile: &Profile) -> Result<()> {
        self.metadata.insert(
            Value::String("ygg".into()),
            serde_yaml_ng::to_value(profile)?,
        );
        Ok(())
    }

    /// Canonical v1 activation input: UTF-8 compact JSON, sorted object keys at
    /// every level, exact body/context bytes, explicit nulls. Display metadata,
    /// provenance, usage counters, state and approval itself are excluded.
    /// Identity and all matching dimensions are included to prevent transplant.
    pub fn approval_input(&self) -> Result<Vec<u8>> {
        let p = self
            .profile()?
            .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
        let input = serde_json::json!({
            "version": 1, "type": self.document_type()?, "id": p.id,
            "scope": p.scope, "repo": p.repo, "user_id": p.user_id,
            "context": p.context, "file_glob": p.file_glob,
            "rule_id": p.rule_id, "scope_tags": p.scope_tags, "body": self.body,
        });
        Ok(serde_json::to_vec(&input)?)
    }

    pub fn approval_digest(&self) -> Result<String> {
        Ok(digest(&self.approval_input()?))
    }

    /// The caller must establish authority to approve. Import cannot use this
    /// operation to carry activation across a different trust domain.
    pub fn activate(
        &mut self,
        corpus_id: Uuid,
        kind: ActivationKind,
        actor: Option<Uuid>,
        at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        ensure!(
            self.document_type()? == "Engineering Rule",
            "only rules can be activated"
        );
        let mut p = self
            .profile()?
            .ok_or_else(|| anyhow!("Yggdrasil profile required"))?;
        if kind != ActivationKind::Legacy {
            ensure!(at.is_some(), "new activation requires timestamp");
        }
        if kind == ActivationKind::Manual {
            ensure!(p.source == Source::Manual, "proposals require review");
        }
        p.state = State::Active;
        p.approval = Some(Approval {
            kind,
            corpus_id,
            digest: self.approval_digest()?,
            actor,
            at,
            extra: BTreeMap::new(),
        });
        self.set_profile(&p)
    }

    pub fn eligible(&self, trusted_corpus: Option<Uuid>, now: DateTime<Utc>) -> Result<bool> {
        let Some(corpus_id) = trusted_corpus else {
            return Ok(false);
        };
        if self.document_type()? != "Engineering Rule" {
            return Ok(false);
        }
        let Some(profile) = self.profile()? else {
            return Ok(false);
        };
        if profile.state != State::Active {
            return Ok(false);
        }
        if let Some(status) = self.metadata.get("status") {
            match status.as_str() {
                Some("stable" | "draft") => {}
                Some("deprecated") => return Ok(false),
                _ => bail!("invalid OKF status"),
            }
        }
        if let Some(stale_after) = self.metadata.get("stale_after") {
            let text = stale_after
                .as_str()
                .ok_or_else(|| anyhow!("invalid stale_after timestamp"))?;
            if now >= DateTime::parse_from_rfc3339(text)? {
                return Ok(false);
            }
        }
        let Some(approval) = profile.approval else {
            return Ok(false);
        };
        if approval.corpus_id != corpus_id || approval.digest != self.approval_digest()? {
            return Ok(false);
        }
        if approval.kind != ActivationKind::Legacy && approval.at.is_none() {
            return Ok(false);
        }
        if approval.kind == ActivationKind::Manual && profile.source != Source::Manual {
            return Ok(false);
        }
        Ok(true)
    }

    /// Import retains provenance and unknown metadata but clears execution
    /// authority, even when the source describes itself as stable/verified.
    pub fn reset_import_activation(&mut self) -> Result<()> {
        if let Some(mut p) = self.profile()? {
            p.state = State::Pending;
            p.approval = None;
            self.set_profile(&p)?;
        }
        Ok(())
    }
}
