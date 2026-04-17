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
    },

    /// Emit the single-line status for Claude Code's statusLine — reads the
    /// harness JSON payload from stdin, shows context %, tokens, cost (2dp),
    /// cache hit rate, recalls/24h.
    Bar,

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
    },
    /// List tasks (defaults to current repo; pass --all for every repo)
    List {
        #[arg(long)] all: bool,
        #[arg(short, long)] status: Option<String>,
    },
    /// Show tasks with no unsatisfied blockers
    Ready,
    /// Show tasks blocked by another open task
    Blocked,
    /// Show a task by "<prefix>-<seq>" or UUID
    Show { reference: String },
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
    /// Add a label to a task
    Label {
        reference: String,
        label: String,
    },
    /// Count open/in_progress/blocked/closed
    Stats {
        #[arg(long)] all: bool,
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
        Commands::Logs { follow, tail, agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::logs_cmd::execute(&pool, follow, tail, agent.as_deref()).await?;
        }
        Commands::Digest { agent, transcript, now } => {
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
                TaskAction::Create { title, description, kind, priority, acceptance, design, notes, label, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::create(&pool, ygg::cli::task_cmd::CreateOpts {
                        title: &title,
                        description: description.as_deref(),
                        kind: kind.as_deref(),
                        priority,
                        acceptance: acceptance.as_deref(),
                        design: design.as_deref(),
                        notes: notes.as_deref(),
                        labels: &label,
                        agent_name: &agent_name,
                    }).await?;
                }
                TaskAction::List { all, status } => {
                    ygg::cli::task_cmd::list(&pool, all, status.as_deref()).await?;
                }
                TaskAction::Ready => { ygg::cli::task_cmd::ready(&pool).await?; }
                TaskAction::Blocked => { ygg::cli::task_cmd::blocked(&pool).await?; }
                TaskAction::Show { reference } => { ygg::cli::task_cmd::show(&pool, &reference).await?; }
                TaskAction::Update { reference, title, description, priority, kind, acceptance, design, notes, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::update(&pool, &reference,
                        title.as_deref(), description.as_deref(), priority, kind.as_deref(),
                        acceptance.as_deref(), design.as_deref(), notes.as_deref(),
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
                TaskAction::Label { reference, label } => {
                    ygg::cli::task_cmd::label(&pool, &reference, &label).await?;
                }
                TaskAction::Stats { all } => {
                    ygg::cli::task_cmd::stats(&pool, all).await?;
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
        Commands::Trace { last, agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::trace_cmd::execute(&pool, last, agent.as_deref()).await?;
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
