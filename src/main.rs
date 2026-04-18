use clap::{Parser, Subcommand};

/// Accept priority as either an integer (0..=4) or a beads-style `P0`..`P4`.
/// Agents used to bd often guess the `P2` form; this keeps them honest
/// without forcing a migration.
fn parse_priority(s: &str) -> Result<i16, String> {
    let trimmed = s.trim();
    let numeric = if let Some(rest) = trimmed.strip_prefix(['P', 'p']) { rest } else { trimmed };
    let n: i16 = numeric.parse().map_err(|_| format!(
        "priority must be 0..=4 or P0..P4 (got '{s}')"
    ))?;
    if !(0..=4).contains(&n) {
        return Err(format!("priority must be between 0 (critical) and 4 (backlog); got {n}"));
    }
    Ok(n)
}

#[derive(Parser)]
#[command(name = "ygg", version, about = "Yggdrasil — High-density agent orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start ygg — open tmux session with dashboard (default when no command given)
    Up,

    /// Bootstrap dependencies (Postgres, Ollama, migrations, status bar)
    Init {
        /// Show command output for debugging
        #[arg(short, long)]
        verbose: bool,
        /// Skip specific deps (pg, ollama, models, statusbar, pgvector, hooks)
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Clear saved skip decisions and re-prompt everything
        #[arg(long)]
        reset: bool,
        /// PostgreSQL connection URL (overrides DATABASE_URL)
        #[arg(long)]
        database_url: Option<String>,
    },

    /// Run database migrations
    Migrate,

    /// Start an agent run loop
    Run {
        /// Agent name (creates or resumes)
        #[arg(short, long)]
        name: String,
        /// Initial task description
        #[arg(short, long)]
        task: Option<String>,
    },

    /// Spawn a new agent in a tmux window
    Spawn {
        /// Task description for the agent
        #[arg(short, long)]
        task: String,
        /// Agent name (auto-generated if omitted)
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Observe a Claude Code session and ingest into the DAG
    Observe {
        /// Agent name to observe
        #[arg(short, long)]
        agent: String,
    },

    /// Inject directives near the attention cursor (called by hooks)
    Inject {
        /// Agent name
        #[arg(short, long)]
        agent: String,
        /// Current prompt text to embed and use as the similarity query
        #[arg(long)]
        prompt: Option<String>,
    },

    /// Launch the TUI dashboard
    Dashboard,

    /// Recover orphaned agents stuck in active states
    Recover {
        /// Staleness threshold in seconds (default 300)
        #[arg(long, default_value = "300")]
        stale_secs: u64,
    },

    /// Start the background watcher daemon
    Watcher,

    /// Resource lock management
    Lock {
        #[command(subcommand)]
        action: LockAction,
    },

    /// Human interrupt controls
    Interrupt {
        #[command(subcommand)]
        action: InterruptAction,
    },

    /// Show agent / system status (quick text output)
    Status {
        /// Specific agent name
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Live event stream — all hook activity, node writes, locks, digests, similarity hits
    Logs {
        /// Stream live (poll every 300ms)
        #[arg(short, long)]
        follow: bool,
        /// Number of recent events to show on start
        #[arg(long, default_value = "20")]
        tail: i64,
        /// Filter to a specific agent
        #[arg(short, long)]
        agent: Option<String>,
        /// Filter to one or more event kinds (comma-separated)
        #[arg(short, long)]
        kind: Option<String>,
        /// Filter to a specific CC session id
        #[arg(long)]
        session: Option<String>,
    },

    /// Digest a session transcript — extract corrections, write Digest node.
    /// Called by the Stop and PreCompact hooks automatically; can be run
    /// manually as `ygg digest --now` to proactively checkpoint a
    /// long-running session before auto-compaction hits.
    Digest {
        /// Agent name
        #[arg(short, long)]
        agent: Option<String>,
        /// Path to the Claude Code transcript JSONL file
        #[arg(long)]
        transcript: Option<String>,
        /// Find and digest the most recent transcript for this agent
        #[arg(long)]
        now: bool,
        /// Called from the Stop hook — after digesting, mark the session
        /// ended so the dashboard stops showing a ghost ×N badge.
        #[arg(long)]
        stop: bool,
    },

    /// Purge stale rows from locks / sessions / memories / agents. Safe to cron.
    Reap {
        #[arg(long)] locks: bool,
        #[arg(long)] sessions: bool,
        #[arg(long)] memories: bool,
        /// Archive (not delete) agents with no activity in the window.
        /// Archived agents keep their history but disappear from live views.
        #[arg(long)] agents: bool,
        #[arg(long, default_value = "7")] older_than_days: i64,
        #[arg(long)] dry_run: bool,
    },

    /// Manage agent identities (list / archive / unarchive).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Output agent context as markdown (called by SessionStart and PreCompact hooks)
    Prime {
        /// Agent name (defaults to YGG_AGENT_NAME env var or current directory name)
        #[arg(short, long)]
        agent: Option<String>,
        /// Transcript file path (for estimating context pressure)
        #[arg(long)]
        transcript: Option<String>,
    },

    /// Task tracking (replaces beads for this repo)
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// Integrate Yggdrasil into a project — install / update / remove the managed
    /// block in CLAUDE.md and AGENTS.md so agents in this repo know to use `ygg`.
    Integrate {
        /// Remove the managed block (and delete the file if that's all that remains)
        #[arg(long)]
        remove: bool,
        /// Target directory (defaults to current working directory)
        #[arg(long)]
        path: Option<String>,
    },

    /// Aggregate events into an effectiveness report over a time window
    Eval {
        /// Time window in hours (default 24)
        #[arg(long, default_value = "24")]
        hours: i64,
    },

    /// Pressure-test recovery paths: compaction, skip-it, crash. Reports
    /// PASS/FAIL for each scenario with forensic detail. Run periodically
    /// to catch regressions in the memory-survival story.
    RecoveryTest {
        /// compact | skip-it | crash | all (default all)
        #[arg(long, default_value = "all")]
        scenario: String,
        /// Agent to test against (defaults to env / pwd basename)
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Show the full pipeline trace for recent user turns — embed →
    /// retrieve → score → emit → (reference, after digest). Lets you
    /// see what Yggdrasil actually did vs. what you think it did.
    Trace {
        /// Number of recent turns to render (default 5)
        #[arg(long, default_value = "5")]
        last: i64,
        /// Filter to a specific agent
        #[arg(short, long)]
        agent: Option<String>,
        /// Dump the full untruncated hit snippets so you can read exactly
        /// what Yggdrasil prepended to each turn's context.
        #[arg(long)]
        full: bool,
    },

    /// Emit the single-line status for Claude Code's statusLine — reads the
    /// harness JSON payload from stdin, shows context %, tokens, cost (2dp),
    /// cache hit rate, recalls/24h.
    Bar,

    /// Manage task worktrees (click-to-do).
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },

    /// Execute tasks with worktrees + CC sessions (click-to-do).
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },

    /// Retroactively scrub content from already-stored nodes. Use when a
    /// secret slipped past the write-time redactor. See ADR yggdrasil-18.
    Forget {
        /// Delete a specific node by UUID (and its embedding cache entry).
        #[arg(long)]
        node: Option<String>,
        /// Replace a literal substring with `[redacted:manual]` across every node.
        #[arg(long)]
        pattern: Option<String>,
        /// Re-run the secret redactor over every existing node's content.
        /// Useful after adding new patterns.
        #[arg(long)]
        redact_all: bool,
    },

    /// Per-repo activity rollup over a recent window (default: last 7 days).
    Rollup {
        /// Number of days to look back.
        #[arg(short, long, default_value = "7")]
        days: i64,
        /// Restrict to a single repo by task prefix.
        #[arg(short, long)]
        repo: Option<String>,
        /// Output format: text, markdown, or json.
        #[arg(short, long, default_value = "markdown")]
        format: String,
    },

    /// Manage scoped, embedded memories (global / repo / session)
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },

    /// Record that the agent is about to invoke a tool — PreToolUse hook.
    AgentTool {
        /// Tool name (Bash, Edit, Read, …)
        tool: String,
        /// Agent name (defaults to env / pwd basename)
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Persist a durable directive the similarity retriever can surface later
    Remember {
        /// The memory text
        text: Option<String>,
        /// Agent name (defaults to env / pwd basename)
        #[arg(short, long)]
        agent: Option<String>,
        /// If set, list recent remembered directives instead of writing one
        #[arg(long)]
        list: bool,
        /// Maximum number of entries to list
        #[arg(long, default_value = "20")]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Create a plan (epic) in the current repo.
    Create {
        title: String,
        #[arg(short, long)] description: Option<String>,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Add a child task under a plan, optionally with dependencies.
    Add {
        epic: String,
        title: String,
        #[arg(short, long)] description: Option<String>,
        #[arg(short, long)] kind: Option<String>,
        #[arg(long, value_delimiter = ',')] deps: Vec<String>,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Execute a single task: worktree + claim + tmux + CC session.
    Run {
        task: String,
        #[arg(long)] dry_run: bool,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Supervise an epic: walk deps, spawn CC sessions for ready tasks,
    /// poll for status changes, exit when no open tasks remain.
    Supervise {
        epic: String,
        #[arg(short, long, default_value = "1")] parallelism: usize,
        #[arg(long)] dry_run: bool,
        #[arg(long, default_value = "5")] poll_secs: u64,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Pause an epic's supervisor — no new tasks spawn until resumed.
    Pause { epic: String },
    /// Resume a paused epic.
    Resume { epic: String },
    /// Abort: tear down in-progress descendants (archive worktrees,
    /// revert to open) and pause the epic. Recoverable.
    Abort {
        epic: String,
        #[arg(short, long)] agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorktreeAction {
    /// Create (or return existing) worktree for a task.
    Ensure { task: String },
    /// Tear down a task's worktree.
    Rm {
        task: String,
        /// Policy: keep / archive / delete (default: archive)
        #[arg(long, default_value = "archive")]
        policy: String,
        /// Force removal even with uncommitted changes.
        #[arg(long)]
        force: bool,
    },
    /// Show the on-disk root + existing worktrees for this host.
    List,
}

#[derive(Subcommand)]
enum AgentAction {
    /// List agents with last-activity age. By default hides archived.
    List {
        #[arg(long)] all: bool,
    },
    /// Archive an agent (hides from live views, keeps history).
    Archive { name: String },
    /// Restore a previously-archived agent.
    Unarchive { name: String },
    /// Show agents that would be archived by `ygg reap --agents` at the
    /// given staleness threshold. Never mutates.
    Stale {
        #[arg(long, default_value = "14")] older_than_days: i64,
    },
}

#[derive(Subcommand)]
enum MemoryAction {
    /// Create a memory at the given scope (global / repo / session)
    Create {
        text: String,
        #[arg(short, long, default_value = "global")] scope: String,
        #[arg(short, long)] agent: Option<String>,
    },
    /// List memories, optionally filtered by scope
    List {
        #[arg(short, long)] scope: Option<String>,
        #[arg(long, default_value = "20")] limit: i64,
    },
    /// Semantic search across memories visible in the current scope
    Search {
        query: String,
        #[arg(long, default_value = "10")] limit: i64,
    },
    /// Pin a memory so it surfaces first in listings and retrieval
    Pin { id: String },
    /// Unpin a previously-pinned memory
    Unpin { id: String },
    /// Expire a memory after N seconds (useful for temporary scratch)
    Expire { id: String, seconds: i64 },
    /// Delete a memory permanently
    Delete { id: String },
}

#[derive(Subcommand)]
enum LockAction {
    /// Acquire a resource lock
    Acquire {
        /// Resource key (e.g. "file:src/auth/")
        resource: String,
        /// Agent name performing the lock
        #[arg(short, long)]
        agent: String,
    },
    /// Release a resource lock
    Release {
        /// Resource key
        resource: String,
        /// Agent name releasing the lock
        #[arg(short, long)]
        agent: String,
    },
    /// List all active locks
    List,
}

#[derive(Subcommand)]
enum TaskAction {
    /// Create a new task in the current repo
    Create {
        /// Title (required unless --file/--stdin/--body-file is used)
        #[arg(default_value = "")]
        title: String,
        // allow_hyphen_values so "-- item" / "* foo" etc. don't get silently
        // interpreted as new flags (yggdrasil-21).
        #[arg(short, long, allow_hyphen_values = true)] description: Option<String>,
        #[arg(short, long)] kind: Option<String>,
        #[arg(short, long, value_parser = parse_priority)] priority: Option<i16>,
        #[arg(long, allow_hyphen_values = true)] acceptance: Option<String>,
        #[arg(long, allow_hyphen_values = true)] design: Option<String>,
        #[arg(long, allow_hyphen_values = true)] notes: Option<String>,
        #[arg(short, long, value_delimiter = ',')] label: Vec<String>,
        #[arg(short, long)] agent: Option<String>,
        /// Link to an external issue tracker (gh-123, jira-PROJ-42, URL, etc.)
        #[arg(long)] external_ref: Option<String>,
        /// Emit the created task(s) as JSON (for agent consumption)
        #[arg(long)] json: bool,
        /// Parse a markdown file into a task tree (H1=epic, H2=feature, H3/4=task).
        /// Body under each header becomes the description. Parent→child dep edges
        /// are auto-linked so `ygg task ready` surfaces leaves first.
        #[arg(short = 'f', long, value_name = "FILE")] file: Option<std::path::PathBuf>,
        /// Read the description body from a file (single-task mode). Useful when
        /// agents write long specs and shell-escaping gets painful.
        #[arg(long, value_name = "FILE")] body_file: Option<std::path::PathBuf>,
        /// Read the description body from stdin (single-task mode).
        #[arg(long)] stdin: bool,
    },
    /// List tasks (defaults to current repo; pass --all for every repo)
    List {
        #[arg(long)] all: bool,
        #[arg(short, long)] status: Option<String>,
        /// Filter to tasks with ALL of these labels (AND)
        #[arg(short, long, value_delimiter = ',')] label: Vec<String>,
        /// Filter to tasks with ANY of these labels (OR)
        #[arg(long, value_delimiter = ',')] label_any: Vec<String>,
        /// Emit results as JSON array
        #[arg(long)] json: bool,
    },
    /// Show tasks with no unsatisfied blockers
    Ready {
        /// Emit results as JSON array
        #[arg(long)] json: bool,
    },
    /// Show tasks blocked by another open task
    Blocked {
        /// Emit results as JSON array
        #[arg(long)] json: bool,
    },
    /// Surface probable duplicate task pairs via pgvector cosine on the
    /// title+description embedding stored at create time.
    Dupes {
        /// Scan every repo (default: current repo only)
        #[arg(long)] all: bool,
        /// Minimum cosine similarity (0.0–1.0). Default 0.85 — high enough
        /// to keep false positives down, low enough to catch reworded dupes.
        #[arg(long, default_value_t = 0.85)] min_similarity: f64,
        /// Max pairs to return
        #[arg(long, default_value_t = 20)] limit: i64,
        /// Emit results as JSON array
        #[arg(long)] json: bool,
    },
    /// Surface tasks that haven't been touched recently — useful for
    /// triage of abandoned in_progress claims.
    Stale {
        /// Age threshold in days (default 30)
        #[arg(long, default_value_t = 30)] days: i32,
        /// Scan every repo instead of just the current one
        #[arg(long)] all: bool,
        /// Filter to a specific status (e.g. in_progress)
        #[arg(short, long)] status: Option<String>,
        /// Emit results as JSON array
        #[arg(long)] json: bool,
    },
    /// Show a task by "<prefix>-<seq>" or UUID
    Show {
        reference: String,
        /// Emit the task as JSON (includes labels, deps, links)
        #[arg(long)] json: bool,
    },
    /// Update task fields
    Update {
        reference: String,
        #[arg(long, allow_hyphen_values = true)] title: Option<String>,
        #[arg(long, allow_hyphen_values = true)] description: Option<String>,
        #[arg(long, value_parser = parse_priority)] priority: Option<i16>,
        #[arg(long)] kind: Option<String>,
        #[arg(long, allow_hyphen_values = true)] acceptance: Option<String>,
        #[arg(long, allow_hyphen_values = true)] design: Option<String>,
        #[arg(long, allow_hyphen_values = true)] notes: Option<String>,
        /// Set the external ref. Pass empty string to clear.
        #[arg(long)] external_ref: Option<String>,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Claim a task (assignee + in_progress)
    Claim {
        reference: String,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Close a task
    Close {
        reference: String,
        #[arg(short, long)] reason: Option<String>,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Change a task's status
    Status {
        reference: String,
        status: String,
        #[arg(short, long)] reason: Option<String>,
        #[arg(short, long)] agent: Option<String>,
    },
    /// Add a dependency: <task> depends on <blocker>
    Dep {
        task: String,
        blocker: String,
    },
    /// Remove a dependency
    Undep {
        task: String,
        blocker: String,
    },
    /// Label management (add/remove/list/list-all)
    Label {
        #[command(subcommand)]
        action: LabelAction,
    },
    /// Bump a task's relevance by a signed delta (clamped 0..100).
    /// Use when a task turns out to be more (or less) load-bearing than first filed.
    Bump {
        reference: String,
        /// Integer delta (+5, -10, etc). Accepts bare numbers too.
        #[arg(allow_hyphen_values = true)]
        delta: i32,
    },
    /// Record a non-blocking relationship between two tasks
    /// (see-also / superseded-by / duplicate-of / related).
    Link {
        from: String,
        to: String,
        #[arg(short, long, default_value = "see-also")]
        kind: String,
    },
    /// Count open/in_progress/blocked/closed
    Stats {
        #[arg(long)] all: bool,
        /// Emit stats as JSON
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum LabelAction {
    /// Attach a label to a task
    Add {
        reference: String,
        label: String,
    },
    /// Remove a label from a task
    Remove {
        reference: String,
        label: String,
    },
    /// List labels on a specific task
    List {
        reference: String,
        #[arg(long)] json: bool,
    },
    /// List every label in the current repo (or --all repos) with usage counts
    ListAll {
        #[arg(long)] all: bool,
        #[arg(long)] json: bool,
    },
}

#[derive(Subcommand)]
enum InterruptAction {
    /// Take over an agent's session
    TakeOver {
        /// Agent name
        agent: String,
    },
    /// Hand back control with a summary
    HandBack {
        /// Agent name
        agent: String,
        /// Summary of what you did
        summary: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ygg=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    let command = cli.command.unwrap_or(Commands::Up);

    match command {
        Commands::Up => {
            // Start the ygg tmux session with dashboard
            if !ygg::tmux::TmuxManager::is_available().await {
                eprintln!("tmux is not installed. Run: ygg init");
                std::process::exit(1);
            }
            ygg::tmux::TmuxManager::ensure_session().await?;
            println!("ygg session ready. Attach with: tmux attach -t ygg");

            // If already in tmux, just attach
            if std::env::var("TMUX").is_ok() {
                println!("Already in tmux. Switch to ygg: Ctrl-b s → ygg");
            } else {
                // Attach to the session
                let _ = tokio::process::Command::new("tmux")
                    .args(["attach", "-t", "ygg"])
                    .status()
                    .await;
            }
        }
        Commands::Init { verbose, skip, reset, database_url } => {
            if reset {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                let skips_path = std::path::Path::new(&home).join(".config/ygg/skips.json");
                let _ = std::fs::remove_file(&skips_path);
                println!("Saved skip decisions cleared.");
            }
            if let Some(ref url) = database_url {
                unsafe { std::env::set_var("DATABASE_URL", url); }
            }
            ygg::cli::init::execute_with_options(verbose, &skip).await?;
        }
        Commands::Migrate => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::db::run_migrations(&pool).await?;
            println!("Migrations complete.");
        }
        Commands::Run { name, task } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let session_id = ygg::status::new_session_id();
            ygg::cli::run::execute(
                &pool,
                &config,
                &name,
                task.as_deref(),
                &session_id,
            )
            .await?;
        }
        Commands::Spawn { task, name } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::spawn::execute(&pool, &config, &task, name.as_deref()).await?;
        }
        Commands::Observe { agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::observe::execute(&pool, &config, &agent).await?;
        }
        Commands::Inject { agent, prompt } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::inject::execute(&pool, &config, &agent, prompt.as_deref()).await?;
        }
        Commands::Dashboard => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::dashboard_cmd::execute(&pool, &config).await?;
        }
        Commands::Recover { stale_secs } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::recover::execute(&pool, Some(stale_secs)).await?;
        }
        Commands::Watcher => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::watcher_cmd::execute(&pool, &config).await?;
        }
        Commands::Lock { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                LockAction::Acquire { resource, agent } => {
                    ygg::cli::lock_cmd::acquire(&pool, &config, &resource, &agent).await?;
                }
                LockAction::Release { resource, agent } => {
                    ygg::cli::lock_cmd::release(&pool, &config, &resource, &agent).await?;
                }
                LockAction::List => {
                    ygg::cli::lock_cmd::list(&pool, &config).await?;
                }
            }
        }
        Commands::Interrupt { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                InterruptAction::TakeOver { agent } => {
                    ygg::cli::interrupt_cmd::execute_take_over(&pool, &config, &agent).await?;
                }
                InterruptAction::HandBack { agent, summary } => {
                    ygg::cli::interrupt_cmd::execute_hand_back(&pool, &config, &agent, &summary).await?;
                }
            }
        }
        Commands::Status { agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::status_cmd::execute(&pool, agent.as_deref()).await?;
        }
        Commands::Logs { follow, tail, agent, kind, session } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let kinds: Vec<String> = kind
                .map(|k| k.split(',').map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            ygg::cli::logs_cmd::execute(
                &pool, follow, tail, agent.as_deref(),
                if kinds.is_empty() { None } else { Some(kinds) },
                session.as_deref(),
            ).await?;
        }
        Commands::Digest { agent, transcript, now, stop } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;

            let path = match (transcript, now) {
                (Some(p), _) => p,
                (None, true) => {
                    match ygg::cli::digest::find_latest_transcript() {
                        Some(p) => p,
                        None => { eprintln!("no Claude Code transcript found — run with --transcript <path>"); return Ok(()); }
                    }
                }
                (None, false) => { eprintln!("pass --transcript <path> or --now"); return Ok(()); }
            };
            ygg::cli::digest::execute(&pool, &config, &agent_name, &path).await?;
            // Mark the session ended when the Stop hook flow called us —
            // PreCompact continues the same session, so only Stop should end.
            if stop {
                if let Ok(Some(a)) = ygg::models::agent::AgentRepo::new(&pool).get_by_name(&agent_name).await {
                    if let Some(sid) = ygg::models::session::resolve_current_session(
                        &pool, a.agent_id, None
                    ).await {
                        let _ = ygg::models::session::SessionRepo::new(&pool).end(sid).await;
                    }
                }
            }
        }
        Commands::Reap { locks, sessions, memories, agents, older_than_days, dry_run } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            // Default to everything when no specific flag is set.
            let all = !(locks || sessions || memories || agents);
            let mut total: i64 = 0;

            if all || locks {
                let sql = "DELETE FROM locks WHERE expires_at < now() - ($1 || ' days')::interval";
                let n = if dry_run {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*)::bigint FROM locks WHERE expires_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).fetch_one(&pool).await.unwrap_or(0)
                } else {
                    sqlx::query(sql).bind(older_than_days.to_string()).execute(&pool).await?.rows_affected() as i64
                };
                println!("locks:    {} {}", if dry_run { "would delete" } else { "deleted" }, n);
                total += n;
            }
            if all || sessions {
                // Close abandoned sessions (no ended_at but stale updated_at)
                // before we delete. Leaves a digest trail intact.
                let n_closed = if dry_run {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*)::bigint FROM sessions
                          WHERE ended_at IS NULL AND updated_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).fetch_one(&pool).await.unwrap_or(0)
                } else {
                    sqlx::query(
                        "UPDATE sessions SET ended_at = updated_at
                          WHERE ended_at IS NULL AND updated_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).execute(&pool).await?.rows_affected() as i64
                };
                println!("sessions: {} {} abandoned (auto-closed)",
                    if dry_run { "would close" } else { "closed" }, n_closed);
                total += n_closed;
            }
            if all || memories {
                let n = if dry_run {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*)::bigint FROM memories
                          WHERE expires_at IS NOT NULL AND expires_at < now()"
                    ).fetch_one(&pool).await.unwrap_or(0)
                } else {
                    sqlx::query(
                        "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < now()"
                    ).execute(&pool).await?.rows_affected() as i64
                };
                println!("memories: {} {} expired",
                    if dry_run { "would delete" } else { "deleted" }, n);
                total += n;
            }
            if all || agents {
                // Archive-not-delete: keep history intact, just hide from
                // live views. Staleness = no events + no sessions + no
                // live locks in the window.
                let repo = ygg::models::agent::AgentRepo::new(&pool);
                let stale = repo.find_stale(older_than_days).await.unwrap_or_default();
                let n = stale.len() as i64;
                if !dry_run {
                    for a in &stale {
                        let _ = repo.archive(a.agent_id).await;
                    }
                }
                println!("agents:   {} {} stale (no activity in {} days)",
                    if dry_run { "would archive" } else { "archived" },
                    n, older_than_days);
                for a in stale.iter().take(5) {
                    println!("  · {}  ({})", a.agent_name,
                        chrono::Utc::now().signed_duration_since(a.updated_at).num_days());
                }
                total += n;
            }
            println!("total:    {}", total);
        }
        Commands::Agent { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let repo = ygg::models::agent::AgentRepo::new(&pool);
            match action {
                AgentAction::List { all } => {
                    let agents = if all { repo.list_all().await? } else { repo.list().await? };
                    if agents.is_empty() {
                        println!("no agents");
                    } else {
                        println!("{:<24} {:<12} {:<10} {:<10}", "NAME", "PERSONA", "STATE", "AGE");
                        let now = chrono::Utc::now();
                        for a in agents {
                            let age_days = now.signed_duration_since(a.updated_at).num_days();
                            let age = if age_days == 0 { "<1d".to_string() } else { format!("{age_days}d") };
                            let persona = a.persona.as_deref().unwrap_or("—");
                            println!("{:<24} {:<12} {:<10} {:<10}",
                                a.agent_name, persona, a.current_state, age);
                        }
                    }
                }
                AgentAction::Archive { name } => {
                    let agent = repo.get_by_name(&name).await?
                        .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;
                    repo.archive(agent.agent_id).await?;
                    println!("archived '{name}'");
                }
                AgentAction::Unarchive { name } => {
                    let agent = repo.get_by_name(&name).await?
                        .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;
                    repo.unarchive(agent.agent_id).await?;
                    println!("restored '{name}'");
                }
                AgentAction::Stale { older_than_days } => {
                    let stale = repo.find_stale(older_than_days).await?;
                    if stale.is_empty() {
                        println!("no stale agents (threshold: {older_than_days}d)");
                    } else {
                        println!("{} stale agent(s) (no activity in {older_than_days}d):", stale.len());
                        let now = chrono::Utc::now();
                        for a in stale {
                            let age = now.signed_duration_since(a.updated_at).num_days();
                            println!("  · {:<24} idle {age}d", a.agent_name);
                        }
                    }
                }
            }
        }
        Commands::Prime { agent, transcript } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            ygg::cli::prime::execute(&agent_name, transcript.as_deref()).await?;
        }
        Commands::Task { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let default_agent = || {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            };
            match action {
                TaskAction::Create { title, description, kind, priority, acceptance, design, notes, label, agent, external_ref, json, file, body_file, stdin } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    // Mode dispatch — mutually exclusive.
                    if let Some(path) = file {
                        if !title.is_empty() {
                            anyhow::bail!("--file and a positional title are mutually exclusive");
                        }
                        ygg::cli::task_cmd::create_from_markdown(
                            &pool, &path, &agent_name, json,
                        ).await?;
                    } else {
                        // Single-task mode. Body precedence: --stdin > --body-file > --description.
                        let body: Option<String> = if stdin {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf)?;
                            Some(buf)
                        } else if let Some(p) = body_file {
                            Some(std::fs::read_to_string(&p)?)
                        } else {
                            description
                        };
                        if title.is_empty() {
                            anyhow::bail!("title is required unless --file is used");
                        }
                        ygg::cli::task_cmd::create(&pool, ygg::cli::task_cmd::CreateOpts {
                            title: &title,
                            description: body.as_deref(),
                            kind: kind.as_deref(),
                            priority,
                            acceptance: acceptance.as_deref(),
                            design: design.as_deref(),
                            notes: notes.as_deref(),
                            labels: &label,
                            agent_name: &agent_name,
                            external_ref: external_ref.as_deref(),
                            json,
                        }).await?;
                    }
                }
                TaskAction::List { all, status, label, label_any, json } => {
                    ygg::cli::task_cmd::list(&pool, all, status.as_deref(), &label, &label_any, json).await?;
                }
                TaskAction::Ready { json } => { ygg::cli::task_cmd::ready(&pool, json).await?; }
                TaskAction::Blocked { json } => { ygg::cli::task_cmd::blocked(&pool, json).await?; }
                TaskAction::Stale { days, all, status, json } => {
                    ygg::cli::task_cmd::stale(&pool, days, all, status.as_deref(), json).await?;
                }
                TaskAction::Dupes { all, min_similarity, limit, json } => {
                    ygg::cli::task_cmd::dupes(&pool, all, min_similarity, limit, json).await?;
                }
                TaskAction::Show { reference, json } => { ygg::cli::task_cmd::show(&pool, &reference, json).await?; }
                TaskAction::Update { reference, title, description, priority, kind, acceptance, design, notes, external_ref, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    // Empty-string external_ref clears the column; None leaves it alone.
                    let ext_update = external_ref.as_deref().map(|s| if s.is_empty() { None } else { Some(s) });
                    ygg::cli::task_cmd::update(&pool, &reference,
                        title.as_deref(), description.as_deref(), priority, kind.as_deref(),
                        acceptance.as_deref(), design.as_deref(), notes.as_deref(),
                        ext_update,
                        &agent_name).await?;
                }
                TaskAction::Claim { reference, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::claim(&pool, &reference, &agent_name).await?;
                }
                TaskAction::Close { reference, reason, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::close(&pool, &reference, reason.as_deref(), &agent_name).await?;
                }
                TaskAction::Status { reference, status, reason, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::set_status(&pool, &reference, &status, reason.as_deref(), &agent_name).await?;
                }
                TaskAction::Dep { task, blocker } => {
                    ygg::cli::task_cmd::add_dep(&pool, &task, &blocker).await?;
                }
                TaskAction::Undep { task, blocker } => {
                    ygg::cli::task_cmd::remove_dep(&pool, &task, &blocker).await?;
                }
                TaskAction::Bump { reference, delta } => {
                    ygg::cli::task_cmd::bump(&pool, &reference, delta).await?;
                }
                TaskAction::Link { from, to, kind } => {
                    ygg::cli::task_cmd::link(&pool, &from, &to, &kind).await?;
                }
                TaskAction::Label { action } => match action {
                    LabelAction::Add { reference, label } => {
                        ygg::cli::task_cmd::label_add(&pool, &reference, &label).await?;
                    }
                    LabelAction::Remove { reference, label } => {
                        ygg::cli::task_cmd::label_remove(&pool, &reference, &label).await?;
                    }
                    LabelAction::List { reference, json } => {
                        ygg::cli::task_cmd::label_list(&pool, &reference, json).await?;
                    }
                    LabelAction::ListAll { all, json } => {
                        ygg::cli::task_cmd::label_list_all(&pool, all, json).await?;
                    }
                },
                TaskAction::Stats { all, json } => {
                    ygg::cli::task_cmd::stats(&pool, all, json).await?;
                }
            }
        }
        Commands::Integrate { remove, path } => {
            let cwd = path
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            if remove {
                let report = ygg::cli::init_project::remove(&cwd)?;
                println!("Removing Yggdrasil integration block in {}:", cwd.display());
                ygg::cli::init_project::print_report(&report);
            } else {
                let report = ygg::cli::init_project::install(&cwd)?;
                println!("Yggdrasil integration in {}:", cwd.display());
                ygg::cli::init_project::print_report(&report);
            }
        }
        Commands::Eval { hours } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::eval_cmd::execute(&pool, hours).await?;
        }
        Commands::Trace { last, agent, full } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::trace_cmd::execute(&pool, last, agent.as_deref(), full).await?;
        }
        Commands::RecoveryTest { scenario, agent } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir().ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let scenario = ygg::cli::recovery_cmd::Scenario::parse(&scenario)
                .ok_or_else(|| anyhow::anyhow!("unknown scenario — use compact|skip-it|crash|all"))?;
            ygg::cli::recovery_cmd::test(&pool, scenario, &agent_name).await?;
        }
        Commands::Bar => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::bar_cmd::execute(&pool).await?;
        }
        Commands::Plan { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let default_agent = || std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                std::env::current_dir().ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "ygg".to_string())
            });
            match action {
                PlanAction::Create { title, description, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    let _ = ygg::cli::plan_cmd::create(
                        &pool, &title, description.as_deref(), &agent_name,
                    ).await?;
                }
                PlanAction::Add { epic, title, description, kind, deps, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    let _ = ygg::cli::plan_cmd::add(
                        &pool, &epic, &title, description.as_deref(),
                        kind.as_deref(), &deps, &agent_name,
                    ).await?;
                }
                PlanAction::Run { task, dry_run, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::run(
                        &pool, &task, &agent_name, dry_run,
                    ).await?;
                }
                PlanAction::Supervise { epic, parallelism, dry_run, poll_secs, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::supervise(
                        &pool, &epic, &agent_name, parallelism.max(1), dry_run, poll_secs,
                    ).await?;
                }
                PlanAction::Pause { epic } => {
                    ygg::cli::plan_cmd::pause(&pool, &epic).await?;
                }
                PlanAction::Resume { epic } => {
                    ygg::cli::plan_cmd::resume(&pool, &epic).await?;
                }
                PlanAction::Abort { epic, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::abort(&pool, &epic, &agent_name).await?;
                }
            }
        }
        Commands::Worktree { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            // Mirrors the task-cmd resolver so `ygg worktree ensure ygg-abcd`
            // or `yggdrasil-42` both work.
            async fn resolve_id(pool: &sqlx::PgPool, r: &str) -> Result<uuid::Uuid, anyhow::Error> {
                if let Ok(u) = uuid::Uuid::parse_str(r) { return Ok(u); }
                let hex = r.strip_prefix("ygg-").unwrap_or(r);
                if hex.len() >= 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    let m: Vec<uuid::Uuid> = sqlx::query_scalar(
                        "SELECT task_id FROM tasks WHERE task_id::text LIKE $1 LIMIT 5"
                    ).bind(format!("{hex}%")).fetch_all(pool).await?;
                    match m.len() {
                        0 => {}
                        1 => return Ok(m[0]),
                        n => anyhow::bail!("ambiguous '{r}' ({n} matches)"),
                    }
                }
                let (prefix, seq) = r.rsplit_once('-').ok_or_else(
                    || anyhow::anyhow!("expected <prefix>-<seq> or ygg-<hex>"))?;
                let seq: i32 = seq.parse().map_err(|_| anyhow::anyhow!("bad seq: {seq}"))?;
                let repo = ygg::models::repo::RepoRepo::new(pool).get_by_prefix(prefix).await?
                    .ok_or_else(|| anyhow::anyhow!("no repo '{prefix}'"))?;
                let t = ygg::models::task::TaskRepo::new(pool).get_by_ref(repo.repo_id, seq).await?
                    .ok_or_else(|| anyhow::anyhow!("no task {r}"))?;
                Ok(t.task_id)
            }
            match action {
                WorktreeAction::Ensure { task } => {
                    let id = resolve_id(&pool, &task).await?;
                    let wt = ygg::worktree::ensure(&pool, id).await?;
                    println!("{} → {}", wt.task_ref, wt.path.display());
                    println!("branch: {}", wt.branch);
                    println!("base:   {}", wt.base_path.display());
                }
                WorktreeAction::Rm { task, policy, force } => {
                    let id = resolve_id(&pool, &task).await?;
                    let policy = ygg::worktree::parse_policy(&policy)?;
                    ygg::worktree::teardown(&pool, id, policy, force).await?;
                    println!("removed worktree for {task} ({policy:?})");
                }
                WorktreeAction::List => {
                    let root = ygg::worktree::worktree_root()?;
                    println!("root: {}", root.display());
                    if !root.exists() {
                        println!("(no worktrees created yet)");
                    } else {
                        for entry in std::fs::read_dir(&root)? {
                            let entry = entry?;
                            println!("  {}", entry.file_name().to_string_lossy());
                        }
                    }
                }
            }
        }
        Commands::Forget { node, pattern, redact_all } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match (node, pattern, redact_all) {
                (Some(id), _, _) => {
                    let uuid: uuid::Uuid = id.parse().map_err(|_| anyhow::anyhow!("invalid UUID"))?;
                    ygg::cli::forget_cmd::forget_node(&pool, uuid).await?;
                }
                (None, Some(pat), _) => {
                    ygg::cli::forget_cmd::forget_pattern(&pool, &pat).await?;
                }
                (None, None, true) => {
                    ygg::cli::forget_cmd::redact_all(&pool).await?;
                }
                _ => {
                    eprintln!("pass --node <uuid>, --pattern <substring>, or --redact-all");
                }
            }
        }
        Commands::Rollup { days, repo, format } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let fmt = match format.as_str() {
                "text" | "txt" => ygg::cli::rollup_cmd::Format::Text,
                "json" => ygg::cli::rollup_cmd::Format::Json,
                _ => ygg::cli::rollup_cmd::Format::Markdown,
            };
            ygg::cli::rollup_cmd::execute(&pool, days, repo.as_deref(), fmt).await?;
        }
        Commands::Memory { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let agent_name_default = || std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                std::env::current_dir().ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "ygg".to_string())
            });
            // Allow an 8-char prefix or full UUID — memory IDs printed by `list` use the prefix.
            async fn parse_id(pool: &sqlx::PgPool, s: &str) -> Result<uuid::Uuid, anyhow::Error> {
                if let Ok(u) = uuid::Uuid::parse_str(s) { return Ok(u); }
                let matches: Vec<uuid::Uuid> = sqlx::query_scalar(
                    "SELECT memory_id FROM memories WHERE memory_id::text LIKE $1"
                )
                .bind(format!("{s}%"))
                .fetch_all(pool).await?;
                match matches.len() {
                    0 => Err(anyhow::anyhow!("no memory matches id prefix '{s}'")),
                    1 => Ok(matches[0]),
                    n => Err(anyhow::anyhow!("ambiguous id prefix '{s}' ({n} matches)")),
                }
            }
            match action {
                MemoryAction::Create { text, scope, agent } => {
                    let scope = ygg::models::memory::MemoryScope::parse(&scope)
                        .ok_or_else(|| anyhow::anyhow!(
                            "scope must be one of: global, repo, session"
                        ))?;
                    let agent_name = agent.unwrap_or_else(agent_name_default);
                    ygg::cli::memory_cmd::create(&pool, &agent_name, scope, &text).await?;
                }
                MemoryAction::List { scope, limit } => {
                    let scope = scope.as_deref().and_then(ygg::models::memory::MemoryScope::parse);
                    ygg::cli::memory_cmd::list(&pool, scope, limit).await?;
                }
                MemoryAction::Search { query, limit } => {
                    ygg::cli::memory_cmd::search(&pool, &query, limit).await?;
                }
                MemoryAction::Pin { id } => {
                    let uuid = parse_id(&pool, &id).await?;
                    ygg::cli::memory_cmd::pin(&pool, uuid, true).await?;
                }
                MemoryAction::Unpin { id } => {
                    let uuid = parse_id(&pool, &id).await?;
                    ygg::cli::memory_cmd::pin(&pool, uuid, false).await?;
                }
                MemoryAction::Expire { id, seconds } => {
                    let uuid = parse_id(&pool, &id).await?;
                    ygg::cli::memory_cmd::expire(&pool, uuid, seconds).await?;
                }
                MemoryAction::Delete { id } => {
                    let uuid = parse_id(&pool, &id).await?;
                    ygg::cli::memory_cmd::delete(&pool, uuid).await?;
                }
            }
        }
        Commands::AgentTool { tool, agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let agent_name = agent.clone().or_else(|| std::env::var("YGG_AGENT_NAME").ok()).unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "ygg".to_string())
            });
            ygg::cli::agent_cmd::set_tool(&pool, &agent_name, &tool).await?;
        }
        Commands::Remember { text, agent, list, limit } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let agent_name = agent.clone().or_else(|| std::env::var("YGG_AGENT_NAME").ok()).unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "ygg".to_string())
            });
            if list {
                ygg::cli::remember::list(&pool, agent.as_deref(), limit).await?;
            } else {
                let text = text.ok_or_else(|| anyhow::anyhow!("provide text to remember, or pass --list"))?;
                ygg::cli::remember::remember(&pool, &agent_name, &text).await?;
            }
        }
    }

    Ok(())
}
