use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ygg", version, about = "Yggdrasil — High-density agent orchestrator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Bootstrap dependencies (Postgres, Ollama, migrations, status bar)
    Init {
        /// Show command output for debugging
        #[arg(short, long)]
        verbose: bool,
        /// Skip specific deps (pg, ollama, models, statusbar)
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
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

    /// Launch the TUI dashboard
    Dashboard,

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
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { verbose, skip, database_url } => {
            if let Some(ref url) = database_url {
                // SAFETY: single-threaded at this point, before tokio runtime spawns work
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
        Commands::Dashboard => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::dashboard_cmd::execute(&pool, &config).await?;
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
    }

    Ok(())
}
