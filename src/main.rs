use clap::{Parser, Subcommand};

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

    /// Output agent context as markdown (called by SessionStart and PreCompact hooks)
    Prime {
        /// Agent name (defaults to YGG_AGENT_NAME env var or current directory name)
        #[arg(short, long)]
        agent: Option<String>,
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
        Commands::Prime { agent } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            ygg::cli::prime::execute(&agent_name).await?;
        }
    }

    Ok(())
}
