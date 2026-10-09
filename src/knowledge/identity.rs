//! Portable corpus/repository IDs and explicit database bindings. Configuration
//! belongs outside the bundle and must accompany it in backups.
use super::{document::digest, store::KnowledgeStore};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const FILE: &str = "identity.json";

#[derive(Clone, Debug)]
pub struct GitIdentity {
    pub common_dir: PathBuf,
    pub origin: Option<String>,
}

fn git_line(bytes: Vec<u8>) -> Result<String> {
    let mut text = String::from_utf8(bytes)?;
    if text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

impl GitIdentity {
    /// Git's common directory, unlike basename or worktree .git, is shared by
    /// all worktrees. Errors never turn a repo document into global knowledge.
    pub fn discover(cwd: &Path) -> Result<Self> {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()?;
        ensure!(
            output.status.success(),
            "cannot resolve Git common directory"
        );
        let common_dir = PathBuf::from(git_line(output.stdout)?).canonicalize()?;
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["config", "--get", "remote.origin.url"])
            .output()?;
        let origin = match output.status.code() {
            Some(0) => Some(canonical_url(&git_line(output.stdout)?, cwd)?),
            Some(1) => None,
            _ => bail!("cannot read Git origin"),
        };
        Ok(Self { common_dir, origin })
    }
}

/// Canonical aliases exclude passwords. Query/fragment identities require an
/// explicit mapping rather than silently conflating distinct repositories.
/// Known forges equate standard SSH/HTTPS transport. Unknown servers retain
/// scheme, port and SSH user because those can identify different repositories.
pub fn canonical_url(raw: &str, cwd: &Path) -> Result<String> {
    ensure!(!raw.trim().is_empty(), "empty repository URL");
    let expanded;
    let raw = if !raw.contains("://") {
        if let Some((host, path)) = raw
            .split_once(':')
            .filter(|(host, _)| !host.contains('/') && !host.is_empty())
        {
            let server = host.rsplit('@').next().unwrap_or(host).to_ascii_lowercase();
            let home_relative = !path.starts_with('/')
                && !["github.com", "gitlab.com", "bitbucket.org"].contains(&server.as_str());
            expanded = format!(
                "ssh://{host}{}{path}",
                if path.starts_with('/') {
                    ""
                } else if home_relative {
                    "/~/"
                } else {
                    "/"
                }
            );
            &expanded
        } else {
            let path = cwd.join(raw).canonicalize()?;
            return url::Url::from_file_path(path)
                .map(|u| u.to_string())
                .map_err(|_| anyhow::anyhow!("invalid local repository path"));
        }
    } else {
        raw
    };
    // Avoid source excerpts in URL diagnostics (the input may carry secrets).
    let mut url = url::Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid repository URL"))?;
    ensure!(
        ["https", "http", "ssh", "git", "file"].contains(&url.scheme()),
        "unsupported repository URL scheme"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "repository URL query or fragment needs explicit identity mapping"
    );
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("invalid local repository URL"))?
            .canonicalize()?;
        return url::Url::from_file_path(path)
            .map(|u| u.to_string())
            .map_err(|_| anyhow::anyhow!("invalid local repository path"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("repository URL has no host"))?
        .to_ascii_lowercase();
    let path = url.path().to_owned();
    ensure!(
        !path.is_empty() && path != "/",
        "repository URL has no path"
    );
    let standard = match url.scheme() {
        "ssh" => url.port().is_none() || url.port() == Some(22),
        "https" | "http" | "git" => url.port().is_none(),
        _ => false,
    };
    if ["github.com", "gitlab.com", "bitbucket.org"].contains(&host.as_str())
        && standard
        && (url.scheme() != "ssh" || url.username() == "git")
    {
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path).to_owned();
        let path = if host == "github.com" {
            path.to_ascii_lowercase()
        } else {
            path
        };
        return Ok(format!("https://{host}{path}"));
    }
    url.set_password(None)
        .map_err(|_| anyhow::anyhow!("invalid repository credentials"))?;
    if url.scheme() != "ssh" {
        url.set_username("")
            .map_err(|_| anyhow::anyhow!("invalid repository credentials"))?;
    }
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&path);
    Ok(url.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RepoBinding {
    pub id: Uuid,
    pub aliases: BTreeSet<String>,
    pub common_dirs: BTreeSet<PathBuf>,
    /// Source database identity -> legacy repo UUIDs (duplicate legacy rows can
    /// intentionally map to one portable repo). URLs are not DB identity.
    pub databases: BTreeMap<Uuid, BTreeSet<Uuid>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Identities {
    pub version: u32,
    pub corpus_id: Uuid,
    pub trusted: bool,
    #[serde(default)]
    pub approval_leads: BTreeSet<Uuid>,
    pub repos: Vec<RepoBinding>,
}

impl Identities {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported identity configuration version"
        );
        let mut ids = BTreeSet::new();
        let mut aliases = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut mappings = BTreeSet::new();
        for repo in &self.repos {
            ensure!(ids.insert(repo.id), "duplicate portable repository ID");
            for alias in &repo.aliases {
                ensure!(aliases.insert(alias), "ambiguous repository URL alias");
            }
            for path in &repo.common_dirs {
                ensure!(
                    path.is_absolute() && paths.insert(path),
                    "ambiguous Git common directory binding"
                );
            }
            for (db, ids) in &repo.databases {
                for id in ids {
                    ensure!(
                        mappings.insert((db, id)),
                        "ambiguous database repository binding"
                    );
                }
            }
        }
        Ok(())
    }

    fn find(&self, git: &GitIdentity) -> Result<Option<usize>> {
        let matches: Vec<_> = self
            .repos
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.common_dirs.contains(&git.common_dir)
                    || git.origin.as_ref().is_some_and(|o| r.aliases.contains(o))
            })
            .map(|(i, _)| i)
            .collect();
        ensure!(
            matches.len() <= 1,
            "Git path and URL resolve to different repositories; explicit mapping required"
        );
        if let Some(&i) = matches.first() {
            let binding = &self.repos[i];
            if let Some(origin) = &git.origin {
                ensure!(
                    binding.aliases.is_empty() || binding.aliases.contains(origin),
                    "origin changed; add an explicit alias before rebinding"
                );
            }
            Ok(Some(i))
        } else {
            Ok(None)
        }
    }
}

pub struct IdentityRegistry {
    config: KnowledgeStore,
}

impl IdentityRegistry {
    /// The supplied directory is policy/identity configuration, outside bundles.
    pub fn open(config_dir: &Path, create: bool) -> Result<Self> {
        Ok(Self {
            config: KnowledgeStore::open(config_dir, create)?,
        })
    }

    pub fn initialize(&self, trusted: bool) -> Result<Identities> {
        self.config.update_control(FILE, |current| {
            let identities = match current {
                Some(text) => serde_json::from_str(text)?,
                None => Identities {
                    version: 1,
                    corpus_id: Uuid::new_v4(),
                    trusted,
                    approval_leads: BTreeSet::new(),
                    repos: Vec::new(),
                },
            };
            identities.validate()?;
            // Reopening cannot silently change trust.
            ensure!(
                identities.trusted == trusted,
                "corpus trust differs; explicit policy update required"
            );
            Ok((serde_json::to_string_pretty(&identities)?, identities))
        })
    }

    pub fn read(&self) -> Result<(Identities, String)> {
        let text = self
            .config
            .read_control(FILE)?
            .ok_or_else(|| anyhow::anyhow!("knowledge identity not initialized"))?;
        let identities: Identities = serde_json::from_str(&text)?;
        identities.validate()?;
        Ok((identities, digest(text.as_bytes())))
    }

    pub fn resolve(&self, git: &GitIdentity) -> Result<Option<Uuid>> {
        let (identities, _) = self.read()?;
        Ok(identities.find(git)?.map(|i| identities.repos[i].id))
    }

    /// Bind a newly observed checkout, preserving IDs for worktrees and clones.
    /// A changed origin or contradictory URL/path requires explicit repair.
    pub fn bind(&self, git: &GitIdentity) -> Result<Uuid> {
        ensure!(
            git.common_dir.is_absolute(),
            "common directory must be absolute"
        );
        self.config.update_control(FILE, |current| {
            let mut identities: Identities = serde_json::from_str(
                current.ok_or_else(|| anyhow::anyhow!("knowledge identity not initialized"))?,
            )?;
            identities.validate()?;
            let i = match identities.find(git)? {
                Some(i) => i,
                None => {
                    identities.repos.push(RepoBinding {
                        id: Uuid::new_v4(),
                        aliases: BTreeSet::new(),
                        common_dirs: BTreeSet::new(),
                        databases: BTreeMap::new(),
                    });
                    identities.repos.len() - 1
                }
            };
            identities.repos[i]
                .common_dirs
                .insert(git.common_dir.clone());
            if let Some(origin) = &git.origin {
                identities.repos[i].aliases.insert(origin.clone());
            }
            identities.validate()?;
            let id = identities.repos[i].id;
            Ok((serde_json::to_string_pretty(&identities)?, id))
        })
    }

    /// Conditional explicit configuration edit, used for alias repair, mapping
    /// legacy scopes and deployment rebinding. Document UUIDs never change.
    pub fn replace(&self, expected_revision: &str, identities: &Identities) -> Result<()> {
        identities.validate()?;
        self.config.update_control(FILE, |current| {
            let text =
                current.ok_or_else(|| anyhow::anyhow!("knowledge identity not initialized"))?;
            ensure!(
                digest(text.as_bytes()) == expected_revision,
                "identity revision conflict"
            );
            let existing: Identities = serde_json::from_str(text)?;
            ensure!(
                existing.corpus_id == identities.corpus_id,
                "replacing corpus identity requires a new trust domain"
            );
            Ok((serde_json::to_string_pretty(identities)?, ()))
        })
    }

    pub fn from_legacy(&self, database: Uuid, repo: Uuid) -> Result<Uuid> {
        let (identities, _) = self.read()?;
        identities
            .repos
            .iter()
            .find(|r| {
                r.databases
                    .get(&database)
                    .is_some_and(|ids| ids.contains(&repo))
            })
            .map(|r| r.id)
            .ok_or_else(|| anyhow::anyhow!("unmapped legacy repository; explicit mapping required"))
    }
}
