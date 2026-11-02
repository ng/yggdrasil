//! `ygg spawn` launch path with tmux faked: the task travels through a file,
//! never through a typed shell line, and a session stuck at a Claude Code
//! startup dialog is reported instead of idling.
//!
//! Requires an isolated Postgres with migrations applied (agent registration):
//!     DATABASE_URL=postgres://... cargo test --test spawn_launch -- --test-threads=1

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use ygg::cli::spawn::{SpawnOptions, StartupDialog, StuckAtStartup, Terminal, spawn_with};

const NASTY_TASK: &str = "-fix the design's \"quoted\" bits; echo $HOME `id` $(touch /tmp/ygg-pwned)\n\
second line with 'single' quotes\n\ttabbed \\ backslash; exit 1";

struct FakeTerminal {
    screen: String,
    typed: Mutex<Vec<(String, String)>>,
    killed: Mutex<Vec<String>>,
}

impl FakeTerminal {
    fn showing(screen: &str) -> Self {
        Self {
            screen: screen.to_string(),
            typed: Mutex::new(Vec::new()),
            killed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Terminal for FakeTerminal {
    async fn is_available(&self) -> bool {
        true
    }
    async fn create_window(&self, _name: &str) -> Result<(String, String), ygg::YggError> {
        Ok(("%agent".into(), "%sidebar".into()))
    }
    async fn type_line(&self, pane: &str, line: &str) -> Result<(), ygg::YggError> {
        self.typed
            .lock()
            .unwrap()
            .push((pane.to_string(), line.to_string()));
        Ok(())
    }
    async fn capture(&self, _pane: &str) -> Result<String, ygg::YggError> {
        Ok(self.screen.clone())
    }
    fn kill_window(&self, name: &str) {
        self.killed.lock().unwrap().push(name.to_string());
    }
}

struct Repo {
    _temp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    argv_out: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A git repo plus a fake `claude` that records its argv, NUL-separated.
fn repo() -> Repo {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README"), "x").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);

    let bin = root.parent().unwrap().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let argv_out = root.parent().unwrap().join("argv");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{}'\n",
            argv_out.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The typed line needs `sh` and the launcher only `cat`; leaving
        // `setsid` off PATH keeps the fake run off a controlling terminal it
        // doesn't have.
        symlink("/bin/sh", bin.join("sh")).unwrap();
        symlink("/bin/cat", bin.join("cat")).unwrap();
    }
    Repo {
        _temp: temp,
        root,
        bin,
        argv_out,
    }
}

fn opts(claude: &Path, timeout: Duration) -> SpawnOptions {
    SpawnOptions {
        permission_mode: "bypassPermissions".into(),
        trust_worktrees: false,
        startup_timeout: timeout,
        claude_bin: claude.to_string_lossy().into_owned(),
    }
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("isolated DATABASE_URL required");
    ygg::db::create_pool(&url).await.unwrap()
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

const WORKING: &str = "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n";

#[tokio::test]
async fn nasty_task_reaches_claude_argv_intact() {
    let repo = repo();
    let term = FakeTerminal::showing(WORKING);
    let name = unique("nasty");
    let spawned = spawn_with(
        &pool().await,
        &term,
        &repo.root,
        NASTY_TASK,
        Some(&name),
        &opts(&repo.bin.join("claude"), Duration::from_secs(5)),
    )
    .await
    .unwrap();
    assert_eq!(spawned.agent_name, name);
    assert!(spawned.worktree.ends_with(format!(".ygg/worktrees/{name}")));

    let typed = term.typed.lock().unwrap().clone();
    let agent_lines: Vec<&String> = typed
        .iter()
        .filter(|(pane, _)| pane == "%agent")
        .map(|(_, l)| l)
        .collect();
    assert_eq!(agent_lines.len(), 1, "{typed:?}");
    let line = agent_lines[0];
    assert!(
        line.starts_with("sh '") && line.ends_with("/ygg/launch.sh'"),
        "{line}"
    );
    for fragment in ["design", "$HOME", "`id`", "quoted", "\n"] {
        assert!(
            !line.contains(fragment),
            "task text leaked into typed line: {line}"
        );
    }

    // Run exactly what was typed, as the pane's shell would.
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", line])
        .env_clear()
        .env("PATH", &repo.bin)
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let raw = std::fs::read(&repo.argv_out).unwrap();
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8(a.to_vec()).unwrap())
        .collect();
    assert_eq!(
        &argv[..6],
        [
            "--dangerously-skip-permissions",
            "--permission-mode",
            "bypassPermissions",
            "--name",
            name.as_str(),
            "--"
        ]
    );
    assert_eq!(
        argv[6], NASTY_TASK,
        "task must arrive as one intact argument"
    );
    assert_eq!(argv.len(), 7);
    assert!(!Path::new("/tmp/ygg-pwned").exists());

    // The task file lives in the worktree's private git dir, never the tree.
    let status = std::process::Command::new("git")
        .current_dir(&spawned.worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[tokio::test]
async fn session_stuck_at_trust_dialog_is_reported() {
    let repo = repo();
    let term = FakeTerminal::showing(
        " Accessing workspace:\n Quick safety check: Is this a project you created or one you trust?\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel\n",
    );
    let name = unique("stuck");
    let err = spawn_with(
        &pool().await,
        &term,
        &repo.root,
        "do the thing",
        Some(&name),
        &opts(&repo.bin.join("claude"), Duration::from_secs(5)),
    )
    .await
    .err()
    .expect("a session at the trust dialog must not count as spawned");
    let stuck = err.downcast_ref::<StuckAtStartup>().expect("typed error");
    assert_eq!(stuck.dialog, StartupDialog::WorkspaceTrust);
    assert_eq!(stuck.agent, name);
    // `ygg spawn` leaves the window for a human; only the scheduler kills it.
    assert!(term.killed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn slow_start_is_not_a_failure() {
    let repo = repo();
    let term = FakeTerminal::showing("");
    let spawned = spawn_with(
        &pool().await,
        &term,
        &repo.root,
        "slow start",
        Some(&unique("slow")),
        &opts(&repo.bin.join("claude"), Duration::from_millis(600)),
    )
    .await;
    assert!(spawned.is_ok());
}
