use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Repo {
    pub repo_id: Uuid,
    pub canonical_url: Option<String>,
    pub name: String,
    pub task_prefix: String,
    pub local_paths: Vec<String>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct RepoRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> RepoRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Register or update a repo. The canonical_url is the stable identity
    /// anchor: when present it is the SOLE key — we match by URL or insert a
    /// fresh row, and never fall back to matching by prefix. That prevents a
    /// colliding directory/worktree name from aliasing a URL-having checkout
    /// onto a stray or NULL-url row. Only genuinely local (no-remote) repos are
    /// keyed by prefix. Returns the existing row if one matches and appends
    /// `local_path` to the known paths list.
    pub async fn register(
        &self,
        canonical_url: Option<&str>,
        name: &str,
        task_prefix: &str,
        local_path: Option<&str>,
    ) -> Result<Repo, sqlx::Error> {
        if let Some(url) = canonical_url {
            if let Some(existing) = self.get_by_url(url).await? {
                if let Some(p) = local_path {
                    self.append_path(existing.repo_id, p).await?;
                    return self
                        .get(existing.repo_id)
                        .await
                        .map(|r| r.expect("just updated"));
                }
                return Ok(existing);
            }
            return self.insert(Some(url), name, task_prefix, local_path).await;
        }

        // No remote: prefix is the only handle we have for a local-only repo.
        if let Some(existing) = self.get_by_prefix(task_prefix).await? {
            if let Some(p) = local_path {
                self.append_path(existing.repo_id, p).await?;
                return self
                    .get(existing.repo_id)
                    .await
                    .map(|r| r.expect("just updated"));
            }
            return Ok(existing);
        }
        self.insert(None, name, task_prefix, local_path).await
    }

    /// Insert a new repo row. The `(user_id, task_prefix)` unique constraint
    /// means a prefix already taken by a different repo would abort the insert;
    /// suffix the prefix (`name-2`, `name-3`, …) until one is free so a URL is
    /// never dropped just because its slug collides.
    async fn insert(
        &self,
        canonical_url: Option<&str>,
        name: &str,
        desired_prefix: &str,
        local_path: Option<&str>,
    ) -> Result<Repo, sqlx::Error> {
        let paths: Vec<String> = local_path.into_iter().map(String::from).collect();
        let mut prefix = desired_prefix.to_string();
        let mut attempt = 1;
        loop {
            let res = sqlx::query_as::<_, Repo>(
                r#"
                INSERT INTO repos (canonical_url, name, task_prefix, local_paths)
                VALUES ($1, $2, $3, $4)
                RETURNING repo_id, canonical_url, name, task_prefix, local_paths,
                          metadata, created_at, updated_at
                "#,
            )
            .bind(canonical_url)
            .bind(name)
            .bind(&prefix)
            .bind(&paths)
            .fetch_one(self.pool)
            .await;
            match res {
                Ok(repo) => return Ok(repo),
                Err(e)
                    if attempt < 50
                        && e.as_database_error()
                            .map(|d| d.is_unique_violation())
                            .unwrap_or(false) =>
                {
                    attempt += 1;
                    prefix = format!("{desired_prefix}-{attempt}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn get(&self, repo_id: Uuid) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE repo_id = $1"#,
        )
        .bind(repo_id)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_url(&self, url: &str) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE canonical_url = $1"#,
        )
        .bind(url)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_prefix(&self, prefix: &str) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE task_prefix = $1"#,
        )
        .bind(prefix)
        .fetch_optional(self.pool)
        .await
    }

    async fn append_path(&self, repo_id: Uuid, path: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE repos
               SET local_paths = array(SELECT DISTINCT unnest(local_paths || $2::TEXT)),
                   updated_at = now()
               WHERE repo_id = $1"#,
        )
        .bind(repo_id)
        .bind(path)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos ORDER BY name"#,
        )
        .fetch_all(self.pool)
        .await
    }
}

/// Detect the repository for the given directory via `git`.
/// Returns (canonical_url, toplevel_path, basename) if inside a git work tree.
pub fn detect_git_repo(start_dir: &std::path::Path) -> Option<(Option<String>, String, String)> {
    let toplevel = std::process::Command::new("git")
        .args(["-C"])
        .arg(start_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !toplevel.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&toplevel.stdout).trim().to_string();
    if top.is_empty() {
        return None;
    }

    let url = std::process::Command::new("git")
        .args(["-C", &top, "config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    // Identity anchor: the origin URL is stable across worktrees, re-clones,
    // and renamed checkouts, so it drives the repo name whenever present. The
    // local directory basename is unstable — a linked worktree's basename is
    // the worktree name (e.g. `.claude/worktrees/ses-debug`) — and is only used
    // when there is no remote at all.
    let name = url
        .as_deref()
        .and_then(name_from_url)
        .or_else(|| main_worktree_basename(&top))
        .unwrap_or_else(|| {
            std::path::Path::new(&top)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string())
        });

    Some((url, top, name))
}

/// Derive a repo name from a git remote URL's terminal path segment. Stable
/// across worktrees, clones, and renames. e.g.
/// `https://github.com/Canairy-Inc/canairy-client.git` -> `canairy-client`,
/// `git@github.com:ng/noctune-core.git` -> `noctune-core`.
fn name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let seg = trimmed
        .rsplit(|c| c == '/' || c == ':')
        .next()
        .unwrap_or(trimmed);
    let seg = seg.strip_suffix(".git").unwrap_or(seg);
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

/// For a repo with no remote, resolve the MAIN working tree's basename so a
/// linked worktree doesn't masquerade as its own repo. `git rev-parse
/// --git-common-dir` returns the shared git dir (the main checkout's `.git`)
/// even from a linked worktree; its parent is the true repo root.
fn main_worktree_basename(top: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", top, "rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if common.is_empty() {
        return None;
    }
    let common_path = std::path::Path::new(&common);
    let abs = if common_path.is_absolute() {
        common_path.to_path_buf()
    } else {
        std::path::Path::new(top).join(common_path)
    };
    abs.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::{name_from_url, slugify};

    #[test]
    fn url_name_is_terminal_segment() {
        assert_eq!(
            name_from_url("https://github.com/Canairy-Inc/canairy-client.git").as_deref(),
            Some("canairy-client")
        );
        assert_eq!(
            name_from_url("git@github.com:ng/noctune-core.git").as_deref(),
            Some("noctune-core")
        );
        // no .git suffix, trailing slash tolerated
        assert_eq!(
            name_from_url("https://github.com/ng/cr-proxy/").as_deref(),
            Some("cr-proxy")
        );
    }

    #[test]
    fn url_name_survives_slugify_stably() {
        // A linked worktree dir ("ses-debug") must not feed the prefix when a
        // remote is present — the URL segment does.
        let name = name_from_url("https://github.com/Canairy-Inc/canairy-client.git").unwrap();
        assert_eq!(slugify(&name), "canairy-client");
    }
}

/// Slugify a repo name into a safe task_prefix: lowercase, alnum + dash only.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "repo".to_string()
    } else {
        out
    }
}
