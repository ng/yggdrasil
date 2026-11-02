use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::AppConfig;
use crate::models::agent::AgentRepo;
use crate::tmux::TmuxManager;

/// How long `ygg spawn` watches a new pane for Claude Code to come up before
/// giving up on classifying it. `YGG_SPAWN_STARTUP_TIMEOUT_SECS` overrides.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(45);
const STARTUP_POLL: Duration = Duration::from_millis(500);

/// The terminal operations `ygg spawn` needs. Production uses tmux; tests
/// substitute a fake so the launch path runs without tmux or `claude`.
#[async_trait::async_trait]
pub trait Terminal: Send + Sync {
    async fn is_available(&self) -> bool;
    /// Open the agent's window; returns `(agent_pane, sidebar_pane)`.
    async fn create_window(&self, name: &str) -> Result<(String, String), crate::YggError>;
    /// Type one line into a pane and press Enter.
    async fn type_line(&self, pane: &str, line: &str) -> Result<(), crate::YggError>;
    async fn capture(&self, pane: &str) -> Result<String, crate::YggError>;
    fn kill_window(&self, name: &str);
}

pub struct Tmux;

#[async_trait::async_trait]
impl Terminal for Tmux {
    async fn is_available(&self) -> bool {
        TmuxManager::is_available().await
    }

    async fn create_window(&self, name: &str) -> Result<(String, String), crate::YggError> {
        // Pane *ids* (tmux's stable @N references) rather than positional
        // .0/.1 — the latter break under `pane-base-index 1` in .tmux.conf.
        TmuxManager::ensure_session().await?;
        TmuxManager::create_task_window(name).await
    }

    async fn type_line(&self, pane: &str, line: &str) -> Result<(), crate::YggError> {
        TmuxManager::send_line(pane, line).await
    }

    async fn capture(&self, pane: &str) -> Result<String, crate::YggError> {
        TmuxManager::capture_pane(pane).await
    }

    fn kill_window(&self, name: &str) {
        TmuxManager::kill_agent_window(name);
    }
}

/// Knobs for one spawn, read from the environment (the user `.env` is loaded
/// into it by `AppConfig::from_env`).
pub struct SpawnOptions {
    /// `YGG_SPAWN_PERMISSION_MODE` (default `bypassPermissions`).
    pub permission_mode: String,
    /// `YGG_SPAWN_TRUST_WORKTREES` — opt-in pre-acceptance of Claude Code's
    /// workspace-trust dialog for the new worktree. See `trust`.
    pub trust_worktrees: bool,
    pub startup_timeout: Duration,
    pub claude_bin: String,
}

impl SpawnOptions {
    pub fn from_env() -> Result<Self, anyhow::Error> {
        let permission_mode = std::env::var("YGG_SPAWN_PERMISSION_MODE")
            .unwrap_or_else(|_| "bypassPermissions".into());
        const ALLOWED_PERM_MODES: &[&str] = &[
            "bypassPermissions",
            "dontAsk",
            "acceptEdits",
            "default",
            "plan",
        ];
        if !ALLOWED_PERM_MODES.contains(&permission_mode.as_str()) {
            anyhow::bail!(
                "invalid YGG_SPAWN_PERMISSION_MODE {:?} — allowed: {}",
                permission_mode,
                ALLOWED_PERM_MODES.join(", ")
            );
        }
        let trust_worktrees = std::env::var("YGG_SPAWN_TRUST_WORKTREES")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false);
        let startup_timeout = std::env::var("YGG_SPAWN_STARTUP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
        Ok(Self {
            permission_mode,
            trust_worktrees,
            startup_timeout,
            claude_bin: "claude".into(),
        })
    }
}

/// A launched agent.
pub struct Spawned {
    pub agent_id: Uuid,
    pub agent_name: String,
    pub worktree: PathBuf,
}

/// The spawned session is waiting at a Claude Code startup dialog instead of
/// working on its task. Its window is left open; callers decide whether to
/// kill it (the scheduler does) or leave it for a human (`ygg spawn` does).
#[derive(Debug, thiserror::Error)]
#[error(
    "agent '{agent}' is stuck at Claude Code's {dialog} in {worktree}; attach with \
     `tmux attach -t ygg` (window '{agent}') to answer it{hint}",
    worktree = worktree.display(),
    hint = if *dialog == StartupDialog::WorkspaceTrust {
        ", or set YGG_SPAWN_TRUST_WORKTREES=1 to pre-accept trust for ygg worktrees of trusted repos"
    } else {
        ""
    }
)]
pub struct StuckAtStartup {
    pub agent: String,
    pub dialog: StartupDialog,
    pub worktree: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupDialog {
    WorkspaceTrust,
    /// Any other selection dialog (MCP server approval, bypass-permissions
    /// acknowledgement, feature announcements), named by its first line.
    Other(String),
}

impl std::fmt::Display for StartupDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkspaceTrust => write!(f, "workspace trust dialog"),
            Self::Other(title) => write!(f, "startup dialog \"{title}\""),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupState {
    /// Claude Code's main UI is up (working on the task, or idle at input).
    Ready,
    Blocked(StartupDialog),
    /// Neither yet — still starting.
    Starting,
}

/// Classify a captured pane during startup. Dialogs are checked first: they
/// render a `❯` selection cursor too, so only the footer text of the main UI
/// counts as ready.
pub fn classify_startup(pane: &str) -> StartupState {
    const TRUST: &[&str] = &[
        "Yes, I trust this folder",
        "Do you trust the files in this folder",
        "Is this a project you created or one you trust",
    ];
    const READY: &[&str] = &["esc to interrupt", "shift+tab to cycle", "? for shortcuts"];
    if TRUST.iter().any(|m| pane.contains(m)) {
        return StartupState::Blocked(StartupDialog::WorkspaceTrust);
    }
    if pane.contains("Enter to confirm") {
        // The dialog is drawn below a `────` rule; anything above it (the
        // shell line that started claude, the banner) is not its title.
        let lines: Vec<&str> = pane.lines().map(str::trim).collect();
        let footer = lines
            .iter()
            .rposition(|l| l.contains("Enter to confirm"))
            .unwrap_or(lines.len());
        let is_rule = |l: &str| l.chars().count() >= 3 && l.chars().all(|c| c == '─');
        let start = lines[..footer]
            .iter()
            .rposition(|l| is_rule(l))
            .map_or(0, |i| i + 1);
        let title = lines[start..footer]
            .iter()
            .find(|l| !l.is_empty())
            .copied()
            .unwrap_or("unknown")
            .to_string();
        return StartupState::Blocked(StartupDialog::Other(title));
    }
    if READY.iter().any(|m| pane.contains(m)) {
        return StartupState::Ready;
    }
    StartupState::Starting
}

/// Spawn a new Claude Code agent in a tmux window (`ygg spawn`).
///
/// Each agent gets its own git worktree so concurrent agents never collide
/// on the shared working directory.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    task: &str,
    name: Option<&str>,
) -> Result<(), anyhow::Error> {
    spawn_detached(pool, config, task, name).await.map(|_| ())
}

/// `execute` for callers that need the spawned agent's id (the scheduler).
pub async fn spawn_detached(
    pool: &sqlx::PgPool,
    _config: &AppConfig,
    task: &str,
    name: Option<&str>,
) -> Result<Spawned, anyhow::Error> {
    let opts = SpawnOptions::from_env()?;
    let base = std::env::current_dir()?;
    spawn_with(pool, &Tmux, &base, task, name, &opts).await
}

pub async fn spawn_with(
    pool: &sqlx::PgPool,
    term: &impl Terminal,
    base: &Path,
    task: &str,
    name: Option<&str>,
    opts: &SpawnOptions,
) -> Result<Spawned, anyhow::Error> {
    let agent_name = match name {
        Some(n) => n.to_string(),
        None => slugify(task),
    };

    if agent_name.is_empty() {
        anyhow::bail!(
            "agent name cannot be empty — provide --name or a task with alphanumeric characters"
        );
    }
    if agent_name.chars().any(|c| c.is_control() || c == '/') {
        anyhow::bail!("agent name {agent_name:?} must not contain '/' or control characters");
    }

    println!("Spawning agent '{agent_name}' for task: {task}");

    if !term.is_available().await {
        anyhow::bail!("tmux is not available. Install tmux first.");
    }

    // Register agent in DB
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = agent_repo.register(&agent_name).await?;
    println!("  Agent ID: {}", agent.agent_id);

    // Create a git worktree so this agent has an isolated working copy.
    let worktree_path = create_worktree(base, &agent_name).await?;
    println!("  Worktree: {}", worktree_path.display());

    if opts.trust_worktrees {
        match trust::pre_trust_spawn_worktree(base, &worktree_path) {
            Ok(outcome) => println!("  Trust:    {outcome}"),
            Err(e) => println!("  Trust:    not pre-accepted ({e})"),
        }
    }

    // The task never passes through a shell string: it is written to a file
    // in the worktree's private git dir, and a generated launcher reads it
    // into claude's argv. Only that launcher's quoted path is typed into the
    // pane, so no task text can break quoting or send-keys.
    let files = write_launch_files(&worktree_path, &agent_name, task, opts).await?;

    let (left_pane, right_pane) = term.create_window(&agent_name).await?;
    term.type_line(&left_pane, &launch_line(&files.launcher))
        .await?;

    // Start the ygg observer in the sidebar (right) pane. It tails the
    // Claude Code JSONL transcript and ingests turns into the DAG.
    let ygg_bin = std::env::current_exe()
        .unwrap_or_else(|_| "ygg".into())
        .to_string_lossy()
        .to_string();
    let observe_cmd = format!(
        "'{}' observe --agent '{}'",
        shell_escape(&ygg_bin),
        shell_escape(&agent_name),
    );
    term.type_line(&right_pane, &observe_cmd).await?;

    match wait_for_startup(term, &left_pane, opts.startup_timeout).await {
        StartupState::Ready => println!("  Claude Code is up and working on the task"),
        StartupState::Blocked(dialog) => {
            return Err(StuckAtStartup {
                agent: agent_name,
                dialog,
                worktree: worktree_path,
            }
            .into());
        }
        StartupState::Starting => println!(
            "  warning: Claude Code did not report ready within {}s; check the window",
            opts.startup_timeout.as_secs()
        ),
    }

    println!("  Agent '{agent_name}' spawned in tmux");
    println!("  ├─ pane 0: Claude Code (agent)");
    println!("  ├─ pane 1: ygg observe (DAG ingest)");
    println!("  └─ worktree: {}", worktree_path.display());
    println!();
    println!("  tmux attach -t ygg");

    Ok(Spawned {
        agent_id: agent.agent_id,
        agent_name,
        worktree: worktree_path,
    })
}

/// Poll the agent pane until Claude Code is up, stuck at a dialog, or the
/// timeout passes. Capture failures count as still starting.
async fn wait_for_startup(term: &impl Terminal, pane: &str, timeout: Duration) -> StartupState {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(screen) = term.capture(pane).await {
            match classify_startup(&screen) {
                StartupState::Starting => {}
                settled => return settled,
            }
        }
        if Instant::now() >= deadline {
            return StartupState::Starting;
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

/// Create a git worktree for the agent under `<base>/.ygg/worktrees/<name>`.
/// Returns the absolute path to the new worktree.
async fn create_worktree(base: &Path, agent_name: &str) -> Result<PathBuf, anyhow::Error> {
    let branch_name = format!("ygg/{agent_name}");
    let worktree_dir = base.join(".ygg/worktrees").join(agent_name);

    if worktree_dir.exists() {
        // Reuse existing worktree (agent respawn / retry).
        return Ok(std::fs::canonicalize(&worktree_dir)?);
    }

    std::fs::create_dir_all(base.join(".ygg/worktrees"))?;

    let output = tokio::process::Command::new("git")
        .current_dir(base)
        .args([
            "worktree",
            "add",
            "-b",
            &branch_name,
            &worktree_dir.to_string_lossy(),
            "HEAD",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }

    Ok(std::fs::canonicalize(&worktree_dir)?)
}

pub struct LaunchFiles {
    pub prompt: PathBuf,
    pub launcher: PathBuf,
}

/// Write the task and its launcher to `<worktree git dir>/ygg/`. That
/// directory belongs to git's per-worktree metadata, so the files can never
/// be committed and disappear with `git worktree remove`.
pub async fn write_launch_files(
    worktree: &Path,
    agent_name: &str,
    task: &str,
    opts: &SpawnOptions,
) -> Result<LaunchFiles, anyhow::Error> {
    let out = tokio::process::Command::new("git")
        .current_dir(worktree)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse --absolute-git-dir failed in {}: {}",
            worktree.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("ygg");
    std::fs::create_dir_all(&dir)?;
    let prompt = dir.join("task.md");
    let launcher = dir.join("launch.sh");
    write_private(&prompt, task)?;
    write_private(
        &launcher,
        &build_launcher(worktree, &prompt, agent_name, opts),
    )?;
    Ok(LaunchFiles { prompt, launcher })
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)?.write_all(contents.as_bytes())
}

/// The only text typed into the agent pane: run the launcher with `sh`, which
/// works whatever the user's interactive shell is.
fn launch_line(launcher: &Path) -> String {
    format!("sh '{}'", shell_escape(&launcher.to_string_lossy()))
}

/// The POSIX `sh` script that starts Claude Code for the agent.
///
/// `"$(cat <prompt>)"` hands the file's bytes to claude as one argument; the
/// shell does not re-parse the substituted text, so quotes, `$`, backticks
/// and newlines in the task arrive intact. `--` keeps a task that starts with
/// `-` from being read as an option.
///
/// Two hardening measures for long-running tool calls (e.g. `cargo test`
/// running past Claude Code's default 2-minute Bash timeout):
///
///   - `BASH_DEFAULT_TIMEOUT_MS` / `BASH_MAX_TIMEOUT_MS` raise the Bash
///     tool timeout (30 min default, 2 h ceiling) so slow commands don't
///     get SIGTERM'd out from under the agent.
///   - `setsid` puts `claude` in its own session so a SIGTERM aimed at a
///     timed-out child's process group can't propagate up and kill the
///     agent. macOS ships `setsid` only as a libc call (no CLI), so we
///     resolve it at runtime via `command -v` and fall back to a bare
///     invocation when it's absent (`${SETSID:+...}` expands to nothing).
///     `-w` keeps `claude` in the foreground so the pane stays attached
///     and the watcher's exit detection still works; `-c` hands it the
///     pane's controlling terminal for the interactive TUI.
///
/// `YGG_AGENT_NAME` is exported so the lifecycle hooks resolve this agent by
/// its registered name. Without it the hooks fall back to the cwd basename —
/// the worktree directory — which re-mints stale identities unrelated to the
/// current task. `YGG_SPAWNED` marks this as a spawned worker for the
/// Stop-check + learnings nudge. The exports apply to claude and every hook
/// subprocess it launches.
fn build_launcher(worktree: &Path, prompt: &Path, agent_name: &str, opts: &SpawnOptions) -> String {
    format!(
        "#!/bin/sh\n\
         # Generated by `ygg spawn` for agent '{name}'. The task is in {prompt_path}.\n\
         cd '{wt}' || exit 1\n\
         YGG_AGENT_NAME='{name}'\n\
         YGG_SPAWNED=1\n\
         BASH_DEFAULT_TIMEOUT_MS=1800000\n\
         BASH_MAX_TIMEOUT_MS=7200000\n\
         export YGG_AGENT_NAME YGG_SPAWNED BASH_DEFAULT_TIMEOUT_MS BASH_MAX_TIMEOUT_MS\n\
         SETSID=\"$(command -v setsid)\"\n\
         exec ${{SETSID:+\"$SETSID\" -w -c}} '{bin}' --dangerously-skip-permissions \
         --permission-mode '{pm}' --name '{name}' -- \"$(cat '{prompt}')\"\n",
        name = shell_escape(agent_name),
        prompt_path = prompt.display().to_string().replace('\n', " "),
        wt = shell_escape(&worktree.to_string_lossy()),
        bin = shell_escape(&opts.claude_bin),
        pm = shell_escape(&opts.permission_mode),
        prompt = shell_escape(&prompt.to_string_lossy()),
    )
}

fn slugify(task: &str) -> String {
    task.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

/// Opt-in pre-acceptance of Claude Code's workspace-trust dialog for worktrees
/// `ygg spawn` creates (`YGG_SPAWN_TRUST_WORKTREES=1`).
///
/// Claude Code stores trust as `projects["<path>"].hasTrustDialogAccepted` in
/// its global config (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`);
/// see <https://code.claude.com/docs/en/permissions> ("What runs before you
/// trust a folder"). There is no flag or setting that skips the dialog for an
/// interactive session, so this edits that file — and only for a worktree
/// under `<base>/.ygg/worktrees/` whose repository the user has already
/// trusted. It never extends trust to anything the user has not trusted.
pub mod trust {
    use std::path::{Path, PathBuf};

    #[derive(Debug, PartialEq, Eq)]
    pub enum TrustOutcome {
        AlreadyTrusted,
        PreAccepted,
        RepoNotTrusted(PathBuf),
    }

    impl std::fmt::Display for TrustOutcome {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::AlreadyTrusted => write!(f, "worktree already trusted"),
                Self::PreAccepted => write!(f, "pre-accepted for this worktree (repo is trusted)"),
                Self::RepoNotTrusted(repo) => write!(
                    f,
                    "not pre-accepted: {} is not trusted in Claude Code",
                    repo.display()
                ),
            }
        }
    }

    /// Claude Code's global config file.
    pub fn claude_config_path() -> Option<PathBuf> {
        match std::env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir).join(".claude.json")),
            _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude.json")),
        }
    }

    pub fn pre_trust_spawn_worktree(
        base: &Path,
        worktree: &Path,
    ) -> Result<TrustOutcome, anyhow::Error> {
        let config = claude_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot locate Claude Code's config file"))?;
        let repo_root = repo_toplevel(base)?;
        pre_trust_worktree(&config, base, &repo_root, worktree)
    }

    fn repo_toplevel(dir: &Path) -> Result<PathBuf, anyhow::Error> {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !out.status.success() {
            anyhow::bail!("{} is not inside a git repository", dir.display());
        }
        Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    }

    /// Mark `worktree` trusted in `config` iff it lies under
    /// `<base>/.ygg/worktrees/` and `repo_root` is already trusted. All other
    /// keys are preserved; the file is replaced atomically with its mode.
    pub fn pre_trust_worktree(
        config: &Path,
        base: &Path,
        repo_root: &Path,
        worktree: &Path,
    ) -> Result<TrustOutcome, anyhow::Error> {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let worktree = canonical(worktree);
        let spawn_root = canonical(&base.join(".ygg/worktrees"));
        if !worktree.starts_with(&spawn_root) || worktree == spawn_root {
            anyhow::bail!(
                "{} is not a ygg spawn worktree under {}",
                worktree.display(),
                spawn_root.display()
            );
        }

        let text = std::fs::read_to_string(config)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", config.display()))?;
        let mut root: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", config.display()))?;
        let accepted = |root: &serde_json::Value, key: &str| {
            root.get("projects")
                .and_then(|p| p.get(key))
                .and_then(|p| p.get("hasTrustDialogAccepted"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };

        let repo_keys = [repo_root.to_path_buf(), canonical(repo_root)];
        if !repo_keys
            .iter()
            .any(|k| accepted(&root, &k.to_string_lossy()))
        {
            return Ok(TrustOutcome::RepoNotTrusted(repo_root.to_path_buf()));
        }
        let key = worktree.to_string_lossy().to_string();
        if accepted(&root, &key) {
            return Ok(TrustOutcome::AlreadyTrusted);
        }

        let projects = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", config.display()))?
            .entry("projects")
            .or_insert_with(|| serde_json::json!({}));
        let entry = projects
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("`projects` in {} is not an object", config.display()))?
            .entry(key)
            .or_insert_with(|| serde_json::json!({}));
        entry
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("project entry is not an object"))?
            .insert("hasTrustDialogAccepted".into(), true.into());

        let tmp = config.with_extension(format!("json.ygg-{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
        if let Ok(meta) = std::fs::metadata(config) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, config)?;
        Ok(TrustOutcome::PreAccepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SpawnOptions {
        SpawnOptions {
            permission_mode: "bypassPermissions".into(),
            trust_worktrees: false,
            startup_timeout: Duration::from_secs(1),
            claude_bin: "claude".into(),
        }
    }

    fn launcher(task_name: &str) -> String {
        build_launcher(
            Path::new("/repo/.ygg/worktrees/agent-x"),
            Path::new("/repo/.git/worktrees/agent-x/ygg/task.md"),
            task_name,
            &opts(),
        )
    }

    #[test]
    fn launcher_sets_bash_timeout_env() {
        let sh = launcher("agent-x");
        // Both timeout knobs are exported so claude (and the Bash tool it
        // spawns) inherit them.
        assert!(sh.contains("BASH_DEFAULT_TIMEOUT_MS=1800000"));
        assert!(sh.contains("BASH_MAX_TIMEOUT_MS=7200000"));
        assert!(sh.contains("export YGG_AGENT_NAME YGG_SPAWNED BASH_DEFAULT_TIMEOUT_MS"));
    }

    #[test]
    fn launcher_wraps_in_setsid_when_available() {
        let sh = launcher("agent-x");
        // Resolves setsid at runtime and applies it only when present,
        // so the script still runs on macOS (no setsid CLI).
        assert!(sh.contains("command -v setsid"));
        assert!(sh.contains("${SETSID:+\"$SETSID\" -w -c}"));
    }

    #[test]
    fn launcher_reads_task_from_file_after_double_dash() {
        let sh = launcher("my-agent");
        assert!(sh.contains("--permission-mode 'bypassPermissions'"));
        assert!(sh.contains("--name 'my-agent'"));
        assert!(sh.contains("--dangerously-skip-permissions"));
        assert!(sh.contains("-- \"$(cat '/repo/.git/worktrees/agent-x/ygg/task.md')\""));
        assert!(sh.contains("cd '/repo/.ygg/worktrees/agent-x' || exit 1"));
    }

    #[test]
    fn launcher_exports_agent_identity_env() {
        // Hooks resolve agent identity from YGG_AGENT_NAME; YGG_SPAWNED marks
        // the worker.
        let sh = launcher("agent-x");
        assert!(sh.contains("YGG_AGENT_NAME='agent-x'"));
        assert!(sh.contains("YGG_SPAWNED=1"));
    }

    #[test]
    fn launcher_escapes_single_quotes_in_agent_name() {
        let sh = launcher("agent's-name");
        assert!(sh.contains("--name 'agent'\\''s-name'"));
    }

    #[test]
    fn launch_line_is_the_only_typed_text() {
        let line = launch_line(Path::new("/r/.git/worktrees/it's/ygg/launch.sh"));
        assert_eq!(line, "sh '/r/.git/worktrees/it'\\''s/ygg/launch.sh'");
    }

    const TRUST_SCREEN: &str = "\
────────────────────────────────────────
 Accessing workspace:
 /repo/.ygg/worktrees/agent-x
 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team).
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 ❯ No, exit
   Yes, I trust this folder
 Enter to confirm · Esc to cancel
";

    const MCP_SCREEN: &str = "\
ng@host repo % sh '/repo/.git/worktrees/agent-x/ygg/launch.sh'
────────────────────────────────────────
  New MCP server found in this project: vercel
  MCP servers may execute code or access system resources.
    Use this MCP server
    Use this and all future MCP servers in this project
  ❯ Continue without using this MCP server
  Enter to confirm · Esc to cancel
";

    const WORKING_SCREEN: &str = "\
❯ Run the bash command sleep 8 then reply OK
✳ Cooking…
────────────────────────────────────────
❯
────────────────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt
";

    #[test]
    fn classify_detects_workspace_trust_dialog() {
        assert_eq!(
            classify_startup(TRUST_SCREEN),
            StartupState::Blocked(StartupDialog::WorkspaceTrust)
        );
        assert_eq!(
            classify_startup("Do you trust the files in this folder?"),
            StartupState::Blocked(StartupDialog::WorkspaceTrust)
        );
    }

    #[test]
    fn classify_names_other_startup_dialogs() {
        assert_eq!(
            classify_startup(MCP_SCREEN),
            StartupState::Blocked(StartupDialog::Other(
                "New MCP server found in this project: vercel".into()
            ))
        );
    }

    #[test]
    fn classify_ready_and_starting() {
        assert_eq!(classify_startup(WORKING_SCREEN), StartupState::Ready);
        assert_eq!(
            classify_startup("$ sh '/x/launch.sh'\n"),
            StartupState::Starting
        );
        assert_eq!(classify_startup(""), StartupState::Starting);
    }

    #[test]
    fn stuck_error_names_dialog_and_trust_knob() {
        let err = StuckAtStartup {
            agent: "a1".into(),
            dialog: StartupDialog::WorkspaceTrust,
            worktree: PathBuf::from("/r/.ygg/worktrees/a1"),
        };
        let msg = err.to_string();
        assert!(msg.contains("workspace trust dialog"), "{msg}");
        assert!(msg.contains("YGG_SPAWN_TRUST_WORKTREES=1"), "{msg}");
        let other = StuckAtStartup {
            dialog: StartupDialog::Other("New MCP server found".into()),
            ..err
        };
        assert!(!other.to_string().contains("YGG_SPAWN_TRUST_WORKTREES"));
    }

    mod trust_file {
        use super::super::trust::*;
        use std::path::Path;

        fn setup(config_json: serde_json::Value) -> (tempfile::TempDir, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join(".claude.json");
            std::fs::write(&config, config_json.to_string()).unwrap();
            std::fs::create_dir_all(dir.path().join("repo/.ygg/worktrees/a1")).unwrap();
            (dir, config)
        }

        fn read(config: &Path) -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap()
        }

        #[test]
        fn pre_accepts_worktree_of_trusted_repo_and_keeps_other_keys() {
            let (dir, config) = setup(serde_json::json!({}));
            let repo = std::fs::canonicalize(dir.path().join("repo")).unwrap();
            std::fs::write(
                &config,
                serde_json::json!({
                    "numStartups": 7,
                    "projects": {
                        repo.to_string_lossy(): {"hasTrustDialogAccepted": true, "allowedTools": ["x"]}
                    }
                })
                .to_string(),
            )
            .unwrap();
            let wt = repo.join(".ygg/worktrees/a1");
            assert_eq!(
                pre_trust_worktree(&config, &repo, &repo, &wt).unwrap(),
                TrustOutcome::PreAccepted
            );
            let json = read(&config);
            assert_eq!(json["numStartups"], 7);
            assert_eq!(
                json["projects"][repo.to_string_lossy().as_ref()]["allowedTools"][0],
                "x"
            );
            assert_eq!(
                json["projects"][wt.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
                true
            );
            assert_eq!(
                pre_trust_worktree(&config, &repo, &repo, &wt).unwrap(),
                TrustOutcome::AlreadyTrusted
            );
        }

        #[test]
        fn leaves_config_alone_when_repo_untrusted() {
            let (dir, config) = setup(serde_json::json!({"projects": {}}));
            let repo = std::fs::canonicalize(dir.path().join("repo")).unwrap();
            let before = std::fs::read_to_string(&config).unwrap();
            let outcome =
                pre_trust_worktree(&config, &repo, &repo, &repo.join(".ygg/worktrees/a1")).unwrap();
            assert_eq!(outcome, TrustOutcome::RepoNotTrusted(repo.clone()));
            assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
        }

        #[test]
        fn refuses_paths_outside_spawn_worktrees() {
            let (dir, config) = setup(serde_json::json!({"projects": {}}));
            let repo = std::fs::canonicalize(dir.path().join("repo")).unwrap();
            assert!(pre_trust_worktree(&config, &repo, &repo, &repo).is_err());
            assert!(
                pre_trust_worktree(&config, &repo, &repo, &repo.join(".ygg/worktrees")).is_err()
            );
        }
    }
}
