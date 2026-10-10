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
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        process::CommandExt,
    },
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
    #[serde(default)]
    exact_base: bool,
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

pub struct RecoverySnapshot {
    pub commit: String,
    pub current: super::store::Snapshot,
}
#[derive(Serialize)]
pub struct PendingInfo {
    pub commit: String,
    pub base: String,
    pub exact_base: bool,
    pub changes: Vec<PendingChange>,
}
#[derive(Serialize)]
pub struct PendingChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}
pub enum RecoveryAction {
    Retry,
    Discard,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Confirmed,
    Published,
    ArchivedUnconfirmed,
}
#[derive(Serialize)]
pub struct Recovery {
    pub commit: String,
    pub outcome: RecoveryOutcome,
    pub archive_ref: Option<String>,
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
// A trusted, operation-local pre-push hook checks Git's advertised old OID.
// receive-pack then compares that same OID atomically when applying the update.
// This prevents remote rewinds from turning a stale import into a fast-forward,
// without permitting force-push or executing hooks from the corpus/config.
struct PushGuard(PathBuf);
impl PushGuard {
    fn new(root: &Path, expected: &str) -> Result<Self> {
        oid(expected.as_bytes())?;
        let path = root.join(format!(".push-guard-{}", Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let guard = Self(path);
        let mut hook = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .custom_flags(libc::O_NOFOLLOW)
            .open(guard.0.join("pre-push"))?;
        // expected has been restricted to a full hexadecimal object ID.
        writeln!(
            hook,
            "#!/bin/sh\ncount=0\nwhile read -r local_ref local_oid remote_ref remote_oid; do\n  [ \"$remote_oid\" = \"{expected}\" ] || exit 1\n  count=$((count + 1))\ndone\n[ \"$count\" -eq 1 ]"
        )?;
        hook.sync_all()?;
        Ok(guard)
    }
}
impl Drop for PushGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("pre-push"));
        let _ = std::fs::remove_dir(&self.0);
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
// Only the small, newly generated empty repository passes through this walker.
// Never traverse an existing object store or delete an abandoned initializer.
fn sync_initial_repository(path: &Path, depth: usize, entries: &mut usize) -> Result<()> {
    ensure!(
        depth <= 8 && *entries < 256,
        "Git initialization exceeds bounds"
    );
    *entries += 1;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() || metadata.is_file(),
        "unsafe Git initializer entry"
    );
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            sync_initial_repository(&entry?.path(), depth + 1, entries)?;
        }
    } else {
        ensure!(
            metadata.len() <= 1024 * 1024,
            "Git initializer file exceeds limit"
        );
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .sync_all()?;
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
        match std::fs::symlink_metadata(&repo) {
            Ok(_) => this.validate_repository(&repo)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    this.control.read_control("snapshot.json")?.is_none()
                        && this.pending()?.is_none(),
                    "Git objects missing for retained cache state; restore the cache"
                );
                // Each attempt owns a different directory. A Git child surviving
                // a killed client can finish only its abandoned staging attempt.
                let stage = this.root.join(format!(".init-{}", Uuid::new_v4()));
                std::fs::DirBuilder::new().mode(0o700).create(&stage)?;
                let git_dir = format!("--git-dir={}", stage.display());
                this.checked(&[&git_dir, "init", "--bare", "--template="], &[], None)?;
                this.validate_repository(&stage)?;
                sync_initial_repository(&stage, 0, &mut 0)?;
                std::fs::rename(&stage, &repo)?;
                File::open(&this.root)?.sync_all()?;
            }
            Err(e) => return Err(e.into()),
        }
        Ok(this)
    }
    fn validate_repository(&self, repo: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(repo)?;
        ensure!(metadata.is_dir(), "invalid Git cache directory");
        for name in ["HEAD", "config", "objects", "refs"] {
            let metadata = std::fs::symlink_metadata(repo.join(name))?;
            ensure!(
                if matches!(name, "objects" | "refs") {
                    metadata.is_dir()
                } else {
                    metadata.is_file()
                },
                "invalid Git cache structure; existing objects retained"
            );
        }
        let git_dir = format!("--git-dir={}", repo.display());
        ensure!(
            self.checked(&[&git_dir, "rev-parse", "--is-bare-repository"], &[], None)? == b"true\n",
            "Git cache must be a valid bare repository"
        );
        Ok(())
    }
    fn run(&self, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<Output> {
        self.run_with_expected(args, input, index, None)
    }
    fn run_with_expected(
        &self,
        args: &[&str],
        input: &[u8],
        index: Option<&Path>,
        expected: Option<&str>,
    ) -> Result<Output> {
        ensure!(
            expected.is_none() || args.first() == Some(&"push"),
            "push guard requires push"
        );
        let guard = expected
            .map(|expected| PushGuard::new(&self.root, expected))
            .transpose()?;
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
            .stdin(Stdio::from(stdin.file.try_clone()?))
            .stdout(Stdio::from(stdout.file.try_clone()?))
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(guard) = &guard {
            command
                .arg("-c")
                .arg(format!("core.hooksPath={}", guard.0.display()));
        }
        command.args(args);
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
    /// Read a retained immutable tree for a coordinator's exact-base comparison.
    /// This neither fetches nor establishes that the commit is still current.
    pub(crate) fn snapshot_files(&self, commit: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        ensure!(
            oid(commit.as_bytes())? == commit,
            "full snapshot commit ID required"
        );
        let _lease = self.control.bounded_lock(".shared.lock")?;
        self.files(commit)
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

    /// Recheck the exact remote branch without fetching or changing saved local
    /// objects. The retained paired backup supplies our local transport lease;
    /// remote writers still require fleet quiescence through the final commit.
    pub fn verify_recovery(
        &self,
        recovery: &super::store::PairedBackup,
        expected_commit: &str,
    ) -> Result<()> {
        recovery.verify_corpus_root(&self.control, &self.root)?;
        oid(expected_commit.as_bytes())?;
        ensure!(
            self.pending()?.is_none(),
            "resolve pending shared publication before rollback"
        );
        let state: State = serde_json::from_str(
            &self
                .control
                .read_control("snapshot.json")?
                .ok_or_else(|| anyhow!("no confirmed shared snapshot"))?,
        )?;
        ensure!(
            !state.blocked && state.commit == expected_commit,
            "recovery commit differs from confirmed shared state"
        );
        self.verify_remote_tip(expected_commit)?;
        recovery.verify_corpus_root(&self.control, &self.root)
    }

    fn verify_remote_tip(&self, expected_commit: &str) -> Result<()> {
        let reference = format!("refs/heads/{}", self.config.branch);
        let output = self.checked(
            &[
                "ls-remote",
                "--refs",
                "--exit-code",
                "--",
                &self.config.remote,
                &reference,
            ],
            &[],
            None,
        )?;
        let fields: Vec<_> = std::str::from_utf8(&output)?.split_whitespace().collect();
        ensure!(
            fields.len() == 2
                && fields[1] == reference
                && oid(fields[0].as_bytes())? == expected_commit,
            "remote branch changed since recovery capture"
        );
        Ok(())
    }
    /// Check a frozen staging cache without fetching or rewriting its receipts.
    pub(crate) fn verify_current_snapshot(&self, expected_commit: &str) -> Result<Snapshot> {
        let _lease = self.control.bounded_lock(".shared.lock")?;
        ensure!(
            self.pending()?.is_none(),
            "staging cache contains an unconfirmed publication"
        );
        let snapshot = self.cached()?;
        ensure!(
            snapshot.is_current && snapshot.commit == expected_commit,
            "staging cache differs from expected publication"
        );
        self.verify_remote_tip(expected_commit)?;
        Ok(snapshot)
    }

    /// Refresh before taking the paired backup. Afterward this reads only its
    /// confirmed objects, never a materialized cache or an unconfirmed draft.
    pub fn recovery_snapshot(
        &self,
        recovery: &super::store::PairedBackup,
    ) -> Result<RecoverySnapshot> {
        recovery.verify_corpus_root(&self.control, &self.root)?;
        let snapshot = self.cached()?;
        ensure!(
            snapshot.is_current,
            "shared recovery snapshot is not current"
        );
        self.verify_recovery(recovery, &snapshot.commit)?;
        let commit = snapshot.commit.clone();
        let current = super::backend::recovery_snapshot(&self.root, snapshot)?;
        self.verify_recovery(recovery, &commit)?;
        Ok(RecoverySnapshot { commit, current })
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
    fn pending_changes(&self, pending: &Pending) -> Result<Vec<Change>> {
        let parents = self.checked(
            &["rev-list", "--parents", "-n", "1", &pending.commit],
            &[],
            None,
        )?;
        let parents = std::str::from_utf8(&parents)?
            .split_whitespace()
            .collect::<Vec<_>>();
        ensure!(
            parents == [pending.commit.as_str(), pending.base.as_str()],
            "pending commit parent differs from journal"
        );
        let before = self.files(&pending.base)?;
        let after = self.files(&pending.commit)?;
        let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
        let changes: Vec<_> = paths
            .into_iter()
            .filter(|p| before.get(*p) != after.get(*p))
            .map(|p| Change {
                path: p.clone(),
                expected: before.get(p).map(|b| digest(b)),
                replacement: after.get(p).cloned(),
            })
            .collect();
        ensure!(
            changes.len() <= if pending.exact_base { 40_000 } else { 32 },
            "pending change batch exceeds limit"
        );
        Ok(changes)
    }
    /// Inspect local intent even when the remote or disposable read cache fails.
    pub fn pending_info(&self) -> Result<Option<PendingInfo>> {
        let _lease = self.control.bounded_lock(".shared.lock")?;
        let Some(pending) = self.pending()? else {
            return Ok(None);
        };
        let changes = self
            .pending_changes(&pending)?
            .into_iter()
            .map(|c| PendingChange {
                path: c.path,
                before: c.expected,
                after: c.replacement.as_ref().map(|b| digest(b)),
            })
            .collect();
        Ok(Some(PendingInfo {
            commit: pending.commit,
            base: pending.base,
            exact_base: pending.exact_base,
            changes,
        }))
    }
    /// Explicit recovery is pinned to the inspected commit, so a concurrent new
    /// intent cannot be discarded or republished accidentally.
    pub fn recover(&self, expected_commit: &str, action: RecoveryAction) -> Result<Recovery> {
        ensure!(
            oid(expected_commit.as_bytes())? == expected_commit,
            "full pending commit ID required"
        );
        let _lease = self.control.bounded_lock(".shared.lock")?;
        let pending = self
            .pending()?
            .ok_or_else(|| anyhow!("no pending shared publication"))?;
        ensure!(
            pending.commit == expected_commit,
            "pending commit changed; inspect again"
        );
        let changes = self.pending_changes(&pending)?;
        let remote = self.fetch()?;
        let confirmed_at = Utc::now();
        if self.reachable(&pending.commit, &remote)? {
            self.publish_cache(remote, confirmed_at)?;
            self.clear(&pending.commit)?;
            return Ok(Recovery {
                commit: pending.commit,
                outcome: RecoveryOutcome::Confirmed,
                archive_ref: None,
            });
        }
        match action {
            RecoveryAction::Retry => {
                let receipt = self.change_locked(
                    &changes,
                    pending.exact_base.then_some(pending.base.as_str()),
                    &mut |_| Ok(()),
                )?;
                Ok(Recovery {
                    commit: receipt.commit,
                    outcome: RecoveryOutcome::Published,
                    archive_ref: None,
                })
            }
            RecoveryAction::Discard => {
                let archive_ref = format!("refs/ygg/drafts/{}", pending.commit);
                // Pin before clearing the journal: interruption leaves recoverable
                // bytes, and ordinary Git GC cannot collect the discarded draft.
                self.checked(&["update-ref", &archive_ref, &pending.commit], &[], None)?;
                self.publish_cache(remote, confirmed_at)?;
                self.clear(&pending.commit)?;
                Ok(Recovery {
                    commit: pending.commit,
                    outcome: RecoveryOutcome::ArchivedUnconfirmed,
                    archive_ref: Some(archive_ref),
                })
            }
        }
    }

    /// Publish one complete snapshot against an exact remote commit. This is a
    /// transport primitive for coordinated imports, not migration authorization.
    /// Callers must validate document semantics and retain the source manifest.
    /// Recovery never rebases this whole-tree intent onto another remote state.
    pub fn replace_snapshot(
        &self,
        expected_commit: &str,
        desired: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Receipt> {
        self.replace_snapshot_at(expected_commit, desired, &mut |_| Ok(()))
    }
    pub(crate) fn replace_snapshot_at(
        &self,
        expected_commit: &str,
        desired: &BTreeMap<String, Vec<u8>>,
        prepared: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<Receipt> {
        ensure!(
            oid(expected_commit.as_bytes())? == expected_commit,
            "full base commit ID required"
        );
        ensure!(
            desired.len() <= 20_000,
            "shared snapshot exceeds file limit"
        );
        let mut bytes = 0u64;
        let mut ids = BTreeSet::new();
        for (name, contents) in desired {
            path(name)?;
            // Include the batch object's header/newline overhead in the same
            // output budget used when reading the resulting committed snapshot.
            bytes = bytes
                .checked_add(contents.len() as u64 + 128)
                .ok_or_else(|| anyhow!("shared snapshot exceeds byte limit"))?;
            ensure!(bytes <= MAX_BYTES, "shared snapshot exceeds byte limit");
            if let Some(id) = name
                .rsplit('/')
                .next()
                .and_then(|s| s.strip_suffix(".md"))
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                ensure!(
                    ids.insert(id),
                    "shared snapshot has duplicate document UUID"
                );
            }
        }
        let _lease = self.control.bounded_lock(".shared.lock")?;
        ensure!(
            self.pending()?.is_none(),
            "prior shared publication is uncertain; recover it first"
        );
        let base = self.fetch()?;
        ensure!(
            base == expected_commit,
            "shared snapshot base changed; reload before publication"
        );
        let current = self.files(&base)?;
        if current == *desired {
            self.publish_cache(base.clone(), Utc::now())?;
            return Ok(Receipt { commit: base });
        }
        let paths: BTreeSet<_> = current.keys().chain(desired.keys()).collect();
        let changes: Vec<_> = paths
            .into_iter()
            .filter(|name| current.get(*name) != desired.get(*name))
            .map(|name| Change {
                path: name.clone(),
                expected: current.get(name).map(|bytes| digest(bytes)),
                replacement: desired.get(name).cloned(),
            })
            .collect();
        self.change_locked(&changes, Some(expected_commit), prepared)
    }

    /// Recover the exact retained whole-snapshot commit, never synthesize a new
    /// commit or accept mere reachability from a changed branch tip. A missing
    /// pending record permits read-only confirmation, not another push.
    pub fn resume_snapshot(
        &self,
        expected_base: &str,
        expected_commit: &str,
        desired: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Receipt> {
        ensure!(
            oid(expected_base.as_bytes())? == expected_base
                && oid(expected_commit.as_bytes())? == expected_commit,
            "full snapshot base and publication commit IDs required"
        );
        let _lease = self.control.bounded_lock(".shared.lock")?;
        let pending = self.pending()?;
        if let Some(pending) = &pending {
            ensure!(
                pending.exact_base
                    && pending.base == expected_base
                    && pending.commit == expected_commit,
                "pending snapshot differs from retained publication intent"
            );
        }
        ensure!(
            self.files(expected_commit)? == *desired,
            "retained publication commit differs from complete desired snapshot"
        );
        if expected_commit != expected_base {
            let parents = self.checked(
                &["rev-list", "--parents", "-n", "1", expected_commit],
                &[],
                None,
            )?;
            ensure!(
                std::str::from_utf8(&parents)?
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    == [expected_commit, expected_base],
                "publication commit is not the expected base's direct child"
            );
        }
        let mut remote = self.fetch()?;
        if remote != expected_commit {
            ensure!(
                pending.is_some() && remote == expected_base && expected_commit != expected_base,
                "snapshot remote changed or no retained push intent remains"
            );
            // The hook and receive-pack CAS both use the original base. Even a
            // rejected/uncertain push cannot rebase or manufacture a new commit.
            let _pushed = self.run_with_expected(
                &[
                    "push",
                    "--porcelain",
                    "--",
                    &self.config.remote,
                    &format!("{expected_commit}:refs/heads/{}", self.config.branch),
                ],
                &[],
                None,
                Some(expected_base),
            );
            remote = self.fetch().map_err(|_| {
                anyhow!("snapshot publication uncertain; retained exact pending commit")
            })?;
        }
        ensure!(
            remote == expected_commit,
            "exact publication tip is not confirmed; retained pending evidence"
        );
        self.publish_cache(remote, Utc::now())?;
        if pending.is_some() {
            self.clear(expected_commit)?;
        }
        Ok(Receipt {
            commit: expected_commit.to_owned(),
        })
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
        self.change_locked(changes, None, prepared)
    }
    fn change_locked(
        &self,
        changes: &[Change],
        exact_base: Option<&str>,
        prepared: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<Receipt> {
        for _ in 0..3 {
            let base = self.fetch()?;
            ensure!(
                exact_base.is_none_or(|expected| expected == base),
                "shared snapshot base changed; bulk publication cannot rebase"
            );
            let files = self.files(&base)?;
            for change in changes
                .iter()
                .filter(|c| exact_base.is_none() && c.replacement.is_some())
            {
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
                exact_base: exact_base.is_some(),
            };
            self.control.update_control("pending.json", |_| {
                Ok((serde_json::to_string(&Some(&pending))?, ()))
            })?;
            prepared(&commit)?;
            let pushed = self.run_with_expected(
                &[
                    "push",
                    "--porcelain",
                    "--",
                    &self.config.remote,
                    &format!("{commit}:refs/heads/{}", self.config.branch),
                ],
                &[],
                None,
                exact_base,
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
            // Retain the last complete intent until confirmed or explicitly
            // archived, including if the next retry finds a document conflict.
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
    fn bulk_files() -> BTreeMap<String, Vec<u8>> {
        (0..64)
            .map(|n| (format!("note-{n}.md"), format!("body {n}\n").into_bytes()))
            .collect()
    }
    #[test]
    fn bulk_snapshot_rejects_ambiguous_ids_and_excess_files_before_publication() {
        let (temp, config) = fixture();
        let transport = SharedGit::open(&temp.path().join("bulk"), config.clone()).unwrap();
        let before = transport.refresh().unwrap();
        let id = Uuid::new_v4();
        let ambiguous = BTreeMap::from([
            (format!("global/notes/{id}.md"), b"one".to_vec()),
            (format!("global/learnings/{id}.md"), b"two".to_vec()),
        ]);
        assert!(
            transport
                .replace_snapshot(&before.commit, &ambiguous)
                .is_err()
        );
        let excess = (0..20_001)
            .map(|n| (format!("note-{n}.md"), Vec::new()))
            .collect();
        assert!(transport.replace_snapshot(&before.commit, &excess).is_err());
        assert!(transport.pending_info().unwrap().is_none());
        assert_eq!(transport.refresh().unwrap().commit, before.commit);
    }
    #[test]
    fn bulk_snapshot_is_one_commit_and_does_not_relax_ordinary_changes() {
        let (temp, config) = fixture();
        let transport = SharedGit::open(&temp.path().join("bulk"), config.clone()).unwrap();
        let before = transport.refresh().unwrap();
        let desired = bulk_files();
        let ordinary: Vec<_> = desired
            .iter()
            .map(|(path, bytes)| Change {
                path: path.clone(),
                expected: None,
                replacement: Some(bytes.clone()),
            })
            .collect();
        assert!(transport.change(&ordinary).is_err());
        let receipt = transport
            .replace_snapshot(&before.commit, &desired)
            .unwrap();
        assert_eq!(transport.refresh().unwrap().files, desired);
        assert_eq!(
            git(
                Path::new(&config.remote),
                &["rev-list", "--count", "knowledge"]
            ),
            "2"
        );
        assert!(
            transport
                .replace_snapshot(&before.commit, &desired)
                .is_err()
        );
        assert_eq!(
            transport
                .replace_snapshot(&receipt.commit, &desired)
                .unwrap()
                .commit,
            receipt.commit
        );
        assert_eq!(
            git(
                Path::new(&config.remote),
                &["rev-list", "--count", "knowledge"]
            ),
            "2"
        );
    }
    #[test]
    fn rejected_bulk_snapshot_never_rebases_over_an_independent_remote_edit() {
        let (temp, config) = fixture();
        let transport = SharedGit::open(&temp.path().join("bulk"), config.clone()).unwrap();
        let other = SharedGit::open(&temp.path().join("other"), config.clone()).unwrap();
        let before = transport.refresh().unwrap();
        let result = transport.replace_snapshot_at(&before.commit, &bulk_files(), &mut |_| {
            other.change(&[Change {
                path: "independent.md".into(),
                expected: None,
                replacement: Some(b"keep this\n".to_vec()),
            }])?;
            Ok(())
        });
        assert!(result.is_err());
        let pending = transport.pending_info().unwrap().unwrap();
        assert_eq!(pending.changes.len(), 65);
        assert!(pending.exact_base);
        assert!(
            transport
                .recover(&pending.commit, RecoveryAction::Retry)
                .is_err()
        );
        let snapshot = other.refresh().unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.files["independent.md"], b"keep this\n");
        assert_eq!(
            git(
                Path::new(&config.remote),
                &["rev-list", "--count", "knowledge"]
            ),
            "2"
        );
        transport
            .recover(&pending.commit, RecoveryAction::Discard)
            .unwrap();
        assert!(transport.pending_info().unwrap().is_none());
    }
    #[test]
    fn bulk_snapshot_cannot_overwrite_a_remote_rewind_after_preparation() {
        let (temp, config) = fixture();
        let transport = SharedGit::open(&temp.path().join("bulk"), config.clone()).unwrap();
        let initial = transport.refresh().unwrap();
        transport
            .change(&[edit(b"new base\n", b"original\n")])
            .unwrap();
        let before = transport.refresh().unwrap();
        let remote = Path::new(&config.remote);
        assert!(
            transport
                .replace_snapshot_at(&before.commit, &bulk_files(), &mut |_| {
                    git(
                        remote,
                        &["update-ref", "refs/heads/knowledge", &initial.commit],
                    );
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(git(remote, &["rev-parse", "knowledge"]), initial.commit);
        assert!(transport.pending_info().unwrap().unwrap().exact_base);
    }
    #[test]
    fn uncertain_bulk_publication_reopens_and_confirms_without_repeating() {
        let (temp, config) = fixture();
        let root = temp.path().join("bulk");
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let before = transport.refresh().unwrap();
        let remote = PathBuf::from(&config.remote);
        let hidden = temp.path().join("offline.git");
        assert!(
            transport
                .replace_snapshot_at(&before.commit, &bulk_files(), &mut |commit| {
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
                })
                .is_err()
        );
        let pending = transport.pending_info().unwrap().unwrap();
        drop(transport);
        std::fs::rename(&hidden, &remote).unwrap();
        let reopened = SharedGit::open(&root, config).unwrap();
        assert_eq!(
            reopened.confirm_pending().unwrap().unwrap().commit,
            pending.commit
        );
        assert_eq!(reopened.refresh().unwrap().files, bulk_files());
        assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
    }

    #[test]
    fn recovery_pins_confirmed_objects_and_rejects_remote_changes_outage_and_drafts() {
        let (temp, config) = fixture();
        let root = temp.path().join("recovery-cache");
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let doc = super::super::document::Document::parse(include_str!(
            "../../tests/fixtures/knowledge/rule.md"
        ))
        .unwrap();
        let key = super::super::store::Key::from_document(&doc).unwrap();
        transport
            .change(&[
                // The transport fixture starts with arbitrary Markdown. SQL
                // recovery now requires every visible document to be modeled.
                Change {
                    path: "rule.md".into(),
                    expected: Some(digest(b"original\n")),
                    replacement: None,
                },
                Change {
                    path: key.relative_path().to_str().unwrap().into(),
                    expected: None,
                    replacement: Some(doc.serialize().unwrap().into_bytes()),
                },
            ])
            .unwrap();
        let confirmed = transport.refresh().unwrap();
        let source = KnowledgeStore::open(&root, false).unwrap();
        let policy = KnowledgeStore::open(&temp.path().join("policy"), true).unwrap();
        let saved = source
            .backup_pair_retained(
                &policy,
                &temp.path().join("archive"),
                &temp.path().join("policy-archive"),
            )
            .unwrap();
        let recovered = transport.recovery_snapshot(&saved).unwrap();
        assert_eq!(recovered.commit, confirmed.commit);
        assert_eq!(recovered.current.documents.len(), 1);
        assert_eq!(
            recovered.current.documents[0].revision,
            digest(&confirmed.files[key.relative_path().to_str().unwrap()])
        );
        saved.verify_sources().unwrap();
        // No fetch updates the saved refs/objects if another host changes the tip.
        let moved = temp.path().join("moved-cache");
        std::fs::rename(&root, &moved).unwrap();
        assert!(
            transport
                .verify_recovery(&saved, &recovered.commit)
                .is_err()
        );
        std::fs::rename(&moved, &root).unwrap();
        transport
            .verify_recovery(&saved, &recovered.commit)
            .unwrap();
        let other = SharedGit::open(&temp.path().join("other"), config.clone()).unwrap();
        assert!(other.recovery_snapshot(&saved).is_err());
        other
            .change(&[Change {
                path: key.relative_path().to_str().unwrap().into(),
                expected: Some(digest(doc.serialize().unwrap().as_bytes())),
                replacement: Some(
                    format!("{}\nremote changed\n", doc.serialize().unwrap()).into_bytes(),
                ),
            }])
            .unwrap();
        assert!(
            transport
                .verify_recovery(&saved, &recovered.commit)
                .is_err()
        );
        assert!(transport.recovery_snapshot(&saved).is_err());
        saved.verify_sources().unwrap();
        let remote = PathBuf::from(&config.remote);
        std::fs::rename(&remote, temp.path().join("offline.git")).unwrap();
        assert!(
            transport
                .verify_recovery(&saved, &recovered.commit)
                .is_err()
        );
        std::fs::rename(temp.path().join("offline.git"), &remote).unwrap();
        drop(saved);
        transport.refresh().unwrap();
        let now = transport.cached().unwrap().commit;
        transport
            .control
            .update_control("pending.json", |_| {
                Ok((
                    serde_json::to_string(&Pending {
                        exact_base: false,
                        commit: now.clone(),
                        base: now.clone(),
                    })?,
                    (),
                ))
            })
            .unwrap();
        let saved = source
            .backup_pair_retained(
                &policy,
                &temp.path().join("draft-archive"),
                &temp.path().join("draft-policy"),
            )
            .unwrap();
        assert!(transport.recovery_snapshot(&saved).is_err());
        saved.verify_sources().unwrap();
    }

    #[test]
    fn whole_snapshot_resume_pushes_exact_commit_and_rejects_changed_tip() {
        let (temp, config) = fixture();
        let root = temp.path().join("cache");
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let base = transport.refresh().unwrap().commit;
        let desired = bulk_files();
        assert!(
            transport
                .replace_snapshot_at(&base, &desired, &mut |_| {
                    bail!("interrupted after durable pending commit, before push")
                })
                .is_err()
        );
        let pending = transport.pending().unwrap().unwrap();
        assert_eq!(transport.refresh().unwrap().commit, base);
        let mut wrong = desired.clone();
        wrong.insert("unrelated.md".into(), b"must not push".to_vec());
        assert!(
            transport
                .resume_snapshot(&base, &pending.commit, &wrong)
                .is_err()
        );
        assert!(transport.pending().unwrap().is_some());
        drop(transport);
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let receipt = transport
            .resume_snapshot(&base, &pending.commit, &desired)
            .unwrap();
        assert_eq!(receipt.commit, pending.commit);
        assert!(transport.pending().unwrap().is_none());
        assert_eq!(
            transport
                .resume_snapshot(&base, &pending.commit, &desired)
                .unwrap()
                .commit,
            pending.commit
        );
        let remote = PathBuf::from(&config.remote);
        assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
        // Once pending intent is cleared, even a rewind to the original base
        // cannot authorize another push of the old publication.
        git(&remote, &["update-ref", "refs/heads/knowledge", &base]);
        assert!(
            transport
                .resume_snapshot(&base, &pending.commit, &desired)
                .is_err()
        );
        assert_eq!(git(&remote, &["rev-parse", "knowledge"]), base);
        git(
            &remote,
            &["update-ref", "refs/heads/knowledge", &pending.commit],
        );
        let other = SharedGit::open(&temp.path().join("other"), config).unwrap();
        let later = other
            .change(&[Change {
                path: "later.md".into(),
                expected: None,
                replacement: Some(b"independent later edit".to_vec()),
            }])
            .unwrap();
        assert!(
            transport
                .resume_snapshot(&base, &pending.commit, &desired)
                .is_err()
        );
        assert_eq!(other.refresh().unwrap().commit, later.commit);
        assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "3");
    }

    #[test]
    fn whole_snapshot_resume_confirms_lost_response_without_republishing() {
        let (temp, config) = fixture();
        let root = temp.path().join("cache");
        let transport = SharedGit::open(&root, config.clone()).unwrap();
        let base = transport.refresh().unwrap().commit;
        let desired = bulk_files();
        let remote = PathBuf::from(&config.remote);
        let offline = temp.path().join("offline.git");
        assert!(
            transport
                .replace_snapshot_at(&base, &desired, &mut |commit| {
                    transport.checked(
                        &[
                            "push",
                            &config.remote,
                            &format!("{commit}:refs/heads/knowledge"),
                        ],
                        &[],
                        None,
                    )?;
                    std::fs::rename(&remote, &offline)?;
                    Ok(())
                })
                .is_err()
        );
        let pending = transport.pending().unwrap().unwrap();
        drop(transport);
        std::fs::rename(&offline, &remote).unwrap();
        let transport = SharedGit::open(&root, config).unwrap();
        assert_eq!(
            transport
                .resume_snapshot(&base, &pending.commit, &desired)
                .unwrap()
                .commit,
            pending.commit
        );
        assert_eq!(transport.refresh().unwrap().files, desired);
        assert_eq!(git(&remote, &["rev-list", "--count", "knowledge"]), "2");
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
    #[test]
    fn explicit_recovery_pins_intent_rechecks_digests_and_archives_conflicting_drafts() {
        let (temp, config) = fixture();
        let a = SharedGit::open(&temp.path().join("a"), config.clone()).unwrap();
        let b = SharedGit::open(&temp.path().join("b"), config).unwrap();
        a.refresh().unwrap();
        assert!(
            a.change_at(&[edit(b"draft\n", b"original\n")], &mut |_| bail!(
                "stop before push"
            ))
            .is_err()
        );
        let pending = a.pending_info().unwrap().unwrap();
        assert_eq!(pending.changes.len(), 1);
        assert_eq!(pending.changes[0].before, Some(digest(b"original\n")));
        assert_eq!(pending.changes[0].after, Some(digest(b"draft\n")));
        assert!(a.recover(&"0".repeat(40), RecoveryAction::Discard).is_err());
        assert_eq!(a.pending_info().unwrap().unwrap().commit, pending.commit);
        b.change(&[Change {
            path: "other.md".into(),
            expected: None,
            replacement: Some(b"other host\n".to_vec()),
        }])
        .unwrap();
        let recovered = a.recover(&pending.commit, RecoveryAction::Retry).unwrap();
        assert!(matches!(recovered.outcome, RecoveryOutcome::Published));
        let snapshot = b.refresh().unwrap();
        assert_eq!(snapshot.files["rule.md"], b"draft\n");
        assert_eq!(snapshot.files["other.md"], b"other host\n");
        assert!(a.pending_info().unwrap().is_none());
        assert!(
            a.change_at(&[edit(b"conflicting draft\n", b"draft\n")], &mut |_| bail!(
                "stop before push"
            ))
            .is_err()
        );
        let pending = a.pending_info().unwrap().unwrap();
        b.change(&[edit(b"new remote edit\n", b"draft\n")]).unwrap();
        assert!(a.recover(&pending.commit, RecoveryAction::Retry).is_err());
        assert_eq!(a.pending_info().unwrap().unwrap().commit, pending.commit);
        let archived = a.recover(&pending.commit, RecoveryAction::Discard).unwrap();
        assert!(matches!(
            archived.outcome,
            RecoveryOutcome::ArchivedUnconfirmed
        ));
        assert_eq!(
            oid(&a
                .checked(
                    &["rev-parse", archived.archive_ref.as_deref().unwrap()],
                    &[],
                    None
                )
                .unwrap())
            .unwrap(),
            pending.commit
        );
        assert_eq!(
            a.files(&pending.commit).unwrap()["rule.md"],
            b"conflicting draft\n"
        );
        assert_eq!(b.refresh().unwrap().files["rule.md"], b"new remote edit\n");
        assert!(a.pending_info().unwrap().is_none());
        a.change(&[edit(b"after recovery\n", b"new remote edit\n")])
            .unwrap();
    }

    #[test]
    fn recovery_confirms_already_published_intent_instead_of_repeating_or_undoing_it() {
        let (temp, config) = fixture();
        let a = SharedGit::open(&temp.path().join("cache"), config.clone()).unwrap();
        for action in [RecoveryAction::Retry, RecoveryAction::Discard] {
            let current = a.refresh().unwrap();
            assert!(
                a.change_at(
                    &[edit(b"once\n", &current.files["rule.md"])],
                    &mut |commit| {
                        a.checked(
                            &[
                                "push",
                                &config.remote,
                                &format!("{commit}:refs/heads/knowledge"),
                            ],
                            &[],
                            None,
                        )?;
                        bail!("lost acknowledgment")
                    }
                )
                .is_err()
            );
            let pending = a.pending_info().unwrap().unwrap();
            let head = a.fetch().unwrap();
            let result = a.recover(&pending.commit, action).unwrap();
            assert!(matches!(result.outcome, RecoveryOutcome::Confirmed));
            assert_eq!(result.commit, pending.commit);
            assert_eq!(a.fetch().unwrap(), head);
            assert!(a.pending_info().unwrap().is_none());
        }
    }
    #[test]
    #[ignore = "subprocess helper for interrupted initialization"]
    fn initialization_worker() {
        let root = std::env::var_os("YGG_INIT_FIXTURE").unwrap();
        SharedGit::open(
            Path::new(&root),
            Config {
                version: 1,
                remote: "unused".into(),
                branch: "knowledge".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn abandoned_live_initializer_cannot_replace_recovered_repository() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let ready = temp.path().join("ready");
        let resume = temp.path().join("resume");
        let done = temp.path().join("done");
        let wrapper = bin.join("git");
        std::fs::write(
            &wrapper,
            r#"#!/bin/sh
initializing=false
for arg do
  case "$arg" in
    init) initializing=true ;;
    --git-dir=*) stage=${arg#--git-dir=} ;;
  esac
done
if [ "$initializing" = true ]; then
  printf '%s' "$stage" > "$YGG_INIT_READY.tmp"
  mv "$YGG_INIT_READY.tmp" "$YGG_INIT_READY"
  n=0
  while [ ! -f "$YGG_INIT_RESUME" ]; do
    n=$((n+1))
    if [ "$n" -gt 100 ]; then exit 71; fi
    sleep 0.1
  done
  /usr/bin/git "$@" && touch "$YGG_INIT_DONE"
else
  exec /usr/bin/git "$@"
fi
"#,
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let diagnostics = temp.path().join("initializer.stderr");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "knowledge::shared::tests::initialization_worker",
            ])
            .env("YGG_INIT_FIXTURE", &root)
            .env("YGG_INIT_READY", &ready)
            .env("YGG_INIT_RESUME", &resume)
            .env("YGG_INIT_DONE", &done)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&diagnostics).unwrap())
            .spawn()
            .unwrap();
        // A newly linked test executable can load slowly on a busy native host.
        // Preserve the fault boundary; do not kill it before setup can run.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready.exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if child.try_wait().unwrap().is_none() {
            // The owned child may exit between try_wait and kill; wait still
            // reaps that exact child and the assertions below reject bad setup.
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        assert!(
            ready.exists(),
            "initializer did not reach fault boundary ({status}): {}",
            std::fs::read_to_string(&diagnostics).unwrap()
        );
        let stage = PathBuf::from(std::fs::read_to_string(&ready).unwrap());
        assert!(
            stage.starts_with(root.canonicalize().unwrap())
                && stage
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".init-")
        );
        assert!(!root.join("objects.git").exists());
        let config = Config {
            version: 1,
            remote: "unused".into(),
            branch: "knowledge".into(),
        };
        let recovered = SharedGit::open(&root, config.clone()).unwrap();
        let published = std::fs::read(root.join("objects.git/config")).unwrap();
        let inode = std::fs::metadata(root.join("objects.git")).unwrap().ino();
        std::fs::write(&resume, b"resume").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(done.exists(), "orphan initializer did not finish");
        assert!(stage.join("config").exists());
        assert_eq!(
            std::fs::metadata(root.join("objects.git")).unwrap().ino(),
            inode
        );
        assert_eq!(
            std::fs::read(root.join("objects.git/config")).unwrap(),
            published
        );
        recovered
            .validate_repository(&root.join("objects.git"))
            .unwrap();
        SharedGit::open(&root, config).unwrap();
    }

    #[test]
    fn incomplete_or_missing_existing_objects_fail_without_replacement() {
        let (temp, config) = fixture();
        let root = temp.path().join("cache");
        std::fs::create_dir_all(root.join("objects.git")).unwrap();
        std::fs::write(root.join("objects.git/keep"), b"retained").unwrap();
        assert!(SharedGit::open(&root, config.clone()).is_err());
        assert_eq!(
            std::fs::read(root.join("objects.git/keep")).unwrap(),
            b"retained"
        );
        assert!(!root.join("objects.git/HEAD").exists());
        let root = temp.path().join("missing");
        let store = SharedGit::open(&root, config.clone()).unwrap();
        store.refresh().unwrap();
        std::fs::rename(root.join("objects.git"), root.join("retained.git")).unwrap();
        assert!(SharedGit::open(&root, config).is_err());
        assert!(!root.join("objects.git").exists());
        assert!(root.join("retained.git/objects").exists());
    }
}
