//! Explicit private Git transport. Remote commits are authoritative; cached
//! snapshots and unconfirmed local commits never imply successful publication.
use super::{document::digest, store::KnowledgeStore};
use anyhow::{Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const REMOTE_REF: &str = "refs/ygg/authoritative";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub remote: String,
    pub branch: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    commit: String,
    confirmed_at: DateTime<Utc>,
    #[serde(default)]
    blocked: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    commit: String,
    base: String,
}

pub struct Snapshot {
    pub commit: String,
    pub confirmed_at: DateTime<Utc>,
    pub files: BTreeMap<String, Vec<u8>>,
    pub is_current: bool,
}
impl Snapshot {
    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.confirmed_at);
        self.is_current && age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(60)
    }
}
pub struct Change {
    pub path: String,
    /// None requires absence; Some requires the SHA-256 of exact source bytes.
    pub expected: Option<String>,
    pub replacement: Option<Vec<u8>>,
}
#[derive(Debug)]
pub struct Receipt {
    pub commit: String,
}
pub struct SharedGit {
    root: PathBuf,
    control: KnowledgeStore,
    config: Config,
}

struct Temporary {
    path: PathBuf,
    file: File,
}
impl Temporary {
    fn new(root: &Path) -> Result<Self> {
        let path = root.join(format!(".{}.tmp", Uuid::new_v4()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        Ok(Self { path, file })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
struct Process(std::process::Child, bool);
impl Drop for Process {
    fn drop(&mut self) {
        if !self.1 {
            // Git can have SSH/credential children. They share this isolated group.
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
}
struct Output {
    success: bool,
    bytes: Vec<u8>,
}
fn oid(value: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(value)?.trim();
    ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Git object identity"
    );
    Ok(value.to_owned())
}
fn path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 4096
            && value.split('/').count() <= 32
            && value
                .split('/')
                .all(|p| !p.is_empty() && !p.starts_with('.') && p.len() <= 255)
            && !value.contains('\\')
            && !value.chars().any(char::is_control),
        "unsupported shared bundle path"
    );
    Ok(())
}
impl SharedGit {
    /// This opens a dedicated transport cache, never an existing private corpus.
    /// Initialization fetches nothing and never uploads content.
    pub fn open(root: &Path, config: Config) -> Result<Self> {
        ensure!(
            config.version == 1
                && !config.remote.is_empty()
                && !config.remote.starts_with('-')
                && !config.remote.chars().any(char::is_control),
            "invalid shared transport configuration"
        );
        let control = KnowledgeStore::open(root, true)?;
        let this = Self {
            root: root.canonicalize()?,
            control,
            config,
        };
        let _lease = this.control.bounded_lock(".shared.lock")?;
        this.checked(
            &[
                "check-ref-format",
                &format!("refs/heads/{}", this.config.branch),
            ],
            &[],
            None,
        )?;
        this.control.update_control(".shared-mode.json", |prior| {
            if let Some(prior) = prior {
                ensure!(
                    serde_json::from_str::<Config>(prior)? == this.config,
                    "transport cache belongs to a different remote/branch"
                );
            }
            Ok((serde_json::to_string(&this.config)?, ()))
        })?;
        let repo = this.root.join("objects.git");
        if !repo.try_exists()? {
            this.checked(
                &[
                    "init",
                    "--bare",
                    "--template=",
                    repo.to_str()
                        .ok_or_else(|| anyhow!("non-UTF8 cache path"))?,
                ],
                &[],
                None,
            )?;
        }
        let metadata = std::fs::symlink_metadata(&repo)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "invalid Git cache directory"
        );
        Ok(this)
    }
    fn run(&self, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<Output> {
        ensure!(input.len() as u64 <= MAX_BYTES, "Git input exceeds limit");
        let mut stdin = Temporary::new(&self.root)?;
        stdin.file.write_all(input)?;
        stdin.file.seek(SeekFrom::Start(0))?;
        let mut stdout = Temporary::new(&self.root)?;
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        // Preserve explicit authentication adapters while clearing repository,
        // index and config injection inherited from an enclosing Git command.
        for name in ["GIT_SSH", "GIT_SSH_COMMAND", "GIT_ASKPASS"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if std::env::var_os("GIT_SSH").is_none() && std::env::var_os("GIT_SSH_COMMAND").is_none() {
            command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        command
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_AUTHOR_NAME", "Yggdrasil")
            .env("GIT_AUTHOR_EMAIL", "ygg@localhost")
            .env("GIT_COMMITTER_NAME", "Yggdrasil")
            .env("GIT_COMMITTER_EMAIL", "ygg@localhost")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "fetch.fsckObjects=true",
                "-c",
                "transfer.fsckObjects=true",
                "-c",
                "core.fsync=all",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
            ])
            .arg(format!(
                "--git-dir={}",
                self.root.join("objects.git").display()
            ))
            .args(args)
            .stdin(Stdio::from(stdin.file.try_clone()?))
            .stdout(Stdio::from(stdout.file.try_clone()?))
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let mut process = Process(command.spawn()?, false);
        let began = Instant::now();
        let status = loop {
            ensure!(
                began.elapsed() < Duration::from_secs(30),
                "Git operation timed out; publication may be uncertain"
            );
            ensure!(
                stdout.file.metadata()?.len() <= MAX_BYTES,
                "Git output exceeds limit"
            );
            if let Some(status) = process.0.try_wait()? {
                process.1 = true;
                break status;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        stdout.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        (&mut stdout.file)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "Git output exceeds limit");
        Ok(Output {
            success: status.success(),
            bytes,
        })
    }
    fn checked(&self, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<Vec<u8>> {
        let out = self.run(args, input, index)?;
        ensure!(out.success, "Git operation failed; remote details omitted");
        Ok(out.bytes)
    }
    fn fetch(&self) -> Result<String> {
        self.checked(
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                &self.config.remote,
                &format!("+refs/heads/{}:{REMOTE_REF}", self.config.branch),
            ],
            &[],
            None,
        )?;
        oid(&self.checked(
            &["rev-parse", "--verify", &format!("{REMOTE_REF}^{{commit}}")],
            &[],
            None,
        )?)
    }
    fn files(&self, commit: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        oid(commit.as_bytes())?;
        let listing = self.checked(&["ls-tree", "-r", "-z", "--full-tree", commit], &[], None)?;
        let mut entries = Vec::new();
        let mut request = Vec::new();
        for entry in listing.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let (header, name) = entry.split_at(
                entry
                    .iter()
                    .position(|b| *b == b'\t')
                    .ok_or_else(|| anyhow!("invalid Git tree entry"))?,
            );
            let header = std::str::from_utf8(header)?.split(' ').collect::<Vec<_>>();
            ensure!(
                header.len() == 3 && header[0] == "100644" && header[1] == "blob",
                "shared snapshot contains symlink, executable or submodule"
            );
            let id = oid(header[2].as_bytes())?;
            let name = std::str::from_utf8(&name[1..])?.to_owned();
            path(&name)?;
            request.extend_from_slice(id.as_bytes());
            request.push(b'\n');
            entries.push((name, id));
            ensure!(
                entries.len() <= 20_000,
                "shared snapshot exceeds file limit"
            );
        }
        let output = self.checked(&["cat-file", "--batch"], &request, None)?;
        let mut rest = output.as_slice();
        let mut files = BTreeMap::new();
        for (name, id) in entries {
            let newline = rest
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| anyhow!("truncated Git object header"))?;
            let header = std::str::from_utf8(&rest[..newline])?
                .split(' ')
                .collect::<Vec<_>>();
            ensure!(
                header.len() == 3 && header[0] == id && header[1] == "blob",
                "Git object identity mismatch"
            );
            let size = header[2].parse::<usize>()?;
            rest = &rest[newline + 1..];
            ensure!(
                size < rest.len() && rest[size] == b'\n',
                "truncated Git object body"
            );
            ensure!(
                files.insert(name, rest[..size].to_vec()).is_none(),
                "duplicate Git path"
            );
            rest = &rest[size + 1..];
        }
        ensure!(rest.is_empty(), "unexpected Git object output");
        Ok(files)
    }
    fn publish_cache(&self, commit: String, confirmed_at: DateTime<Utc>) -> Result<Snapshot> {
        let files = self.files(&commit)?;
        let state = State {
            commit: commit.clone(),
            confirmed_at,
            blocked: false,
        };
        self.control.update_control("snapshot.json", |_| {
            Ok((serde_json::to_string(&state)?, ()))
        })?;
        Ok(Snapshot {
            commit,
            confirmed_at: state.confirmed_at,
            files,
            is_current: true,
        })
    }
    pub fn refresh(&self) -> Result<Snapshot> {
        let _lease = self.control.bounded_lock(".shared.lock")?;
        let commit = self.fetch()?;
        self.publish_cache(commit, Utc::now())
    }
    pub fn cached(&self) -> Result<Snapshot> {
        let state: State = serde_json::from_str(
            &self
                .control
                .read_control("snapshot.json")?
                .ok_or_else(|| anyhow!("no confirmed shared snapshot"))?,
        )?;
        let remote = oid(&self.checked(&["rev-parse", "--verify", REMOTE_REF], &[], None)?)?;
        Ok(Snapshot {
            is_current: state.commit == remote && !state.blocked,
            files: self.files(&state.commit)?,
            commit: state.commit,
            confirmed_at: state.confirmed_at,
        })
    }
    pub(crate) fn invalidate_cached(&self, snapshot: &Snapshot) -> Result<()> {
        let _lease = self.control.bounded_lock(".shared.lock")?;
        self.control.update_control("snapshot.json", |prior| {
            let mut state: State =
                serde_json::from_str(prior.ok_or_else(|| anyhow!("no shared snapshot"))?)?;
            if state.commit == snapshot.commit && state.confirmed_at == snapshot.confirmed_at {
                state.blocked = true;
            }
            Ok((serde_json::to_string(&state)?, ()))
        })
    }
    fn pending(&self) -> Result<Option<Pending>> {
        let pending: Option<Pending> = self
            .control
            .read_control("pending.json")?
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .flatten();
        if let Some(pending) = &pending {
            ensure!(
                oid(pending.commit.as_bytes())? == pending.commit
                    && oid(pending.base.as_bytes())? == pending.base,
                "invalid pending Git identity"
            );
        }
        Ok(pending)
    }
    fn clear(&self, commit: &str) -> Result<()> {
        self.control.update_control("pending.json", |prior| {
            let pending: Option<Pending> = prior.map(serde_json::from_str).transpose()?.flatten();
            ensure!(
                pending.is_some_and(|p| p.commit == commit),
                "shared publication journal changed"
            );
            Ok(("null".into(), ()))
        })
    }
    fn reachable(&self, commit: &str, remote: &str) -> Result<bool> {
        Ok(self
            .run(&["merge-base", "--is-ancestor", commit, remote], &[], None)?
            .success)
    }
    /// Recover only confirmed publication. An unconfirmed commit stays a draft;
    /// this operation never repeats an uncertain mutation or force-pushes.
    pub fn confirm_pending(&self) -> Result<Option<Receipt>> {
        let _lease = self.control.bounded_lock(".shared.lock")?;
        let Some(pending) = self.pending()? else {
            return Ok(None);
        };
        let remote = self.fetch()?;
        let confirmed_at = Utc::now();
        ensure!(
            self.reachable(&pending.commit, &remote)?,
            "pending publication is not confirmed; retained local commit for recovery"
        );
        self.publish_cache(remote, confirmed_at)?;
        self.clear(&pending.commit)?;
        Ok(Some(Receipt {
            commit: pending.commit,
        }))
    }
    pub fn change(&self, changes: &[Change]) -> Result<Receipt> {
        self.change_at(changes, &mut |_| Ok(()))
    }
    fn change_at(
        &self,
        changes: &[Change],
        prepared: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<Receipt> {
        ensure!(
            !changes.is_empty() && changes.len() <= 32,
            "invalid shared change batch"
        );
        let mut unique = BTreeSet::new();
        for change in changes {
            path(&change.path)?;
            ensure!(unique.insert(&change.path), "duplicate changed path");
            ensure!(
                change
                    .replacement
                    .as_ref()
                    .is_none_or(|b| b.len() as u64 <= MAX_BYTES),
                "shared change exceeds limit"
            );
        }
        let _lease = self.control.bounded_lock(".shared.lock")?;
        ensure!(
            self.pending()?.is_none(),
            "prior shared publication is uncertain; confirm it before another write"
        );
        for _ in 0..3 {
            let base = self.fetch()?;
            let files = self.files(&base)?;
            for change in changes.iter().filter(|c| c.replacement.is_some()) {
                let id = change
                    .path
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.strip_suffix(".md"))
                    .and_then(|s| Uuid::parse_str(s).ok());
                if let Some(id) = id {
                    for name in files.keys().filter(|p| **p != change.path) {
                        let other = name
                            .rsplit('/')
                            .next()
                            .and_then(|s| s.strip_suffix(".md"))
                            .and_then(|s| Uuid::parse_str(s).ok());
                        ensure!(
                            other != Some(id)
                                || changes
                                    .iter()
                                    .any(|c| c.path == *name && c.replacement.is_none()),
                            "shared document UUID already exists at another path"
                        );
                    }
                }
            }
            for change in changes {
                ensure!(
                    files.get(&change.path).map(|b| digest(b)) == change.expected,
                    "shared document revision conflict; reload before editing"
                );
            }
            let index = Temporary::new(&self.root)?;
            std::fs::remove_file(&index.path)?; // read-tree creates a new index, never a worktree checkout.
            self.checked(&["read-tree", &base], &[], Some(&index.path))?;
            let mut update = Vec::new();
            for change in changes {
                let entry = match &change.replacement {
                    Some(bytes) => format!(
                        "100644 {}\t{}\0",
                        oid(&self.checked(&["hash-object", "-w", "--stdin"], bytes, None)?)?,
                        change.path
                    ),
                    None => format!("0 {}\t{}\0", "0".repeat(base.len()), change.path),
                };
                update.extend_from_slice(entry.as_bytes());
            }
            self.checked(
                &["update-index", "-z", "--index-info"],
                &update,
                Some(&index.path),
            )?;
            let tree = oid(&self.checked(&["write-tree"], &[], Some(&index.path))?)?;
            let commit = oid(&self.checked(
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    &base,
                    "-m",
                    "Update Yggdrasil knowledge",
                ],
                &[],
                None,
            )?)?;
            // Verify the resulting complete snapshot before publishing any commit.
            self.files(&commit)?;
            let pending = Pending {
                commit: commit.clone(),
                base,
            };
            self.control.update_control("pending.json", |_| {
                Ok((serde_json::to_string(&Some(&pending))?, ()))
            })?;
            prepared(&commit)?;
            let pushed = self.run(
                &[
                    "push",
                    "--porcelain",
                    "--",
                    &self.config.remote,
                    &format!("{commit}:refs/heads/{}", self.config.branch),
                ],
                &[],
                None,
            );
            // Always check reachability, including after a lost push response.
            let remote = self
                .fetch()
                .map_err(|_| anyhow!("shared publication uncertain; retained pending commit"))?;
            let confirmed_at = Utc::now();
            if self.reachable(&commit, &remote)? {
                self.publish_cache(remote, confirmed_at)?;
                self.clear(&commit)?;
                return Ok(Receipt { commit });
            }
            let rejected = pushed.is_ok_and(|out| {
                !out.success
                    && out.bytes.split(|b| *b == b'\n').any(|line| {
                        line.starts_with(b"!\t") && line.windows(10).any(|w| w == b"[rejected]")
                    })
            });
            ensure!(
                rejected,
                "shared publication uncertain; retained pending commit"
            );
            self.clear(&commit)?;
            // A definitive non-fast-forward rejection retries from a fresh remote
            // only after rechecking every affected expected digest above.
        }
        bail!("shared branch kept changing; retry after reloading")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    fn fixture() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        let work = temp.path().join("source");
        git(
            temp.path(),
            &["init", "-b", "knowledge", work.to_str().unwrap()],
        );
        std::fs::write(work.join("rule.md"), "original\n").unwrap();
        git(&work, &["add", "rule.md"]);
        git(&work, &["commit", "-m", "initial"]);
        git(
            &work,
            &[
                "push",
                remote.to_str().unwrap(),
                "HEAD:refs/heads/knowledge",
            ],
        );
        let config = Config {
            version: 1,
            remote: remote.to_str().unwrap().into(),
            branch: "knowledge".into(),
        };
        (temp, config)
    }
    fn edit(text: &[u8], expected: &[u8]) -> Change {
        Change {
            path: "rule.md".into(),
            expected: Some(digest(expected)),
            replacement: Some(text.to_vec()),
        }
    }
    #[test]
    fn two_hosts_reject_conflicts_retry_disjoint_changes_and_confirm_remote_reachability() {
        let (temp, config) = fixture();
        let a = SharedGit::open(&temp.path().join("a"), config.clone()).unwrap();
        let b = SharedGit::open(&temp.path().join("b"), config).unwrap();
        let old = a.refresh().unwrap();
        assert!(old.fresh(Utc::now()));
        assert!(!old.fresh(old.confirmed_at + chrono::Duration::seconds(61)));
        assert!(!old.fresh(old.confirmed_at - chrono::Duration::seconds(1)));
        b.change(&[edit(b"host b\n", b"original\n")]).unwrap();
        assert!(a.change(&[edit(b"host a\n", b"original\n")]).is_err());
        assert_eq!(a.refresh().unwrap().files["rule.md"], b"host b\n");
        let mut raced = false;
        let receipt = a
            .change_at(&[edit(b"host a fresh\n", b"host b\n")], &mut |_| {
                if !raced {
                    raced = true;
                    b.change(&[Change {
                        path: "other.md".into(),
                        expected: None,
                        replacement: Some(b"disjoint\n".to_vec()),
                    }])?;
                }
                Ok(())
            })
            .unwrap();
        let snapshot = b.refresh().unwrap();
        assert_eq!(snapshot.commit, receipt.commit);
        assert_eq!(snapshot.files["rule.md"], b"host a fresh\n");
        assert_eq!(snapshot.files["other.md"], b"disjoint\n");
        assert!(a.pending().unwrap().is_none());
        // A multi-path scope move is one remote tree, never a half-moved snapshot.
        a.change(&[
            Change {
                path: "rule.md".into(),
                expected: Some(digest(b"host a fresh\n")),
                replacement: None,
            },
            Change {
                path: "moved/rule.md".into(),
                expected: None,
                replacement: Some(b"moved bytes\n".to_vec()),
            },
        ])
        .unwrap();
        let snapshot = b.refresh().unwrap();
        assert!(!snapshot.files.contains_key("rule.md"));
        assert_eq!(snapshot.files["moved/rule.md"], b"moved bytes\n");
    }
    #[test]
    fn ambiguous_publication_retains_commit_and_restart_confirms_without_repeating_it() {
        let (temp, config) = fixture();
        let root = temp.path().join("cache");
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let original = transport.refresh().unwrap();
        let remote = PathBuf::from(&config.remote);
        let hidden = temp.path().join("offline.git");
        let result =
            transport.change_at(&[edit(b"published once\n", b"original\n")], &mut |commit| {
                transport.checked(
                    &[
                        "push",
                        &config.remote,
                        &format!("{commit}:refs/heads/knowledge"),
                    ],
                    &[],
                    None,
                )?;
                std::fs::rename(&remote, &hidden)?;
                Ok(())
            });
        assert!(result.is_err());
        let pending = transport.pending().unwrap().unwrap();
        assert_eq!(transport.cached().unwrap().commit, original.commit);
        assert!(
            transport
                .change(&[edit(b"must not repeat\n", b"original\n")])
                .is_err()
        );
        drop(transport);
        std::fs::rename(&hidden, &remote).unwrap();
        let reopened = SharedGit::open(&root, config).unwrap();
        let receipt = reopened.confirm_pending().unwrap().unwrap();
        assert_eq!(receipt.commit, pending.commit);
        assert!(reopened.confirm_pending().unwrap().is_none());
        assert_eq!(
            reopened.cached().unwrap().files["rule.md"],
            b"published once\n"
        );
        assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
    }
    #[test]
    fn snapshots_refuse_unsafe_entries_and_do_not_refresh_freshness_on_failure() {
        let (temp, config) = fixture();
        let transport = SharedGit::open(&temp.path().join("cache"), config.clone()).unwrap();
        let valid = transport.refresh().unwrap();
        let source = temp.path().join("source");
        std::os::unix::fs::symlink("/etc/passwd", source.join("escape.md")).unwrap();
        git(&source, &["add", "escape.md"]);
        git(&source, &["commit", "-m", "unsafe symlink"]);
        git(
            &source,
            &["push", &config.remote, "HEAD:refs/heads/knowledge"],
        );
        assert!(transport.refresh().is_err());
        assert_eq!(transport.cached().unwrap().confirmed_at, valid.confirmed_at);
        assert_eq!(transport.cached().unwrap().commit, valid.commit);
        assert!(!transport.cached().unwrap().fresh(Utc::now()));
        assert!(
            transport
                .change(&[Change {
                    path: "../outside".into(),
                    expected: None,
                    replacement: Some(vec![])
                }])
                .is_err()
        );
        let mut changed = config;
        changed.branch = "different".into();
        assert!(SharedGit::open(&temp.path().join("cache"), changed).is_err());
    }
}
