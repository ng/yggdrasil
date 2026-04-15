use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::process::Command;

use crate::config::AppConfig;
use crate::db;
use crate::ollama::OllamaClient;

// Warm gradient: dim → orange → gold
const C_DIM: &str = "\x1b[90m";    // dim gray
const C_MID: &str = "\x1b[38;5;208m"; // orange
const C_HI: &str = "\x1b[38;5;220m";  // gold
const C_OK: &str = "\x1b[38;5;114m";  // soft green
const C_ERR: &str = "\x1b[38;5;203m"; // soft red
const C_WARN: &str = "\x1b[38;5;221m"; // yellow
const C_BOLD: &str = "\x1b[1m";
const C_RST: &str = "\x1b[0m";

/// Detect the system package manager.
enum PackageManager {
    Apt,
    Brew,
}

fn detect_pkg_manager() -> Option<PackageManager> {
    if std::process::Command::new("brew").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        Some(PackageManager::Brew)
    } else if std::process::Command::new("apt-get").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        Some(PackageManager::Apt)
    } else {
        None
    }
}

/// Check if we have sudo access.
async fn has_sudo() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!("  {C_MID}{{spinner}}{C_RST} {{msg}}"))
            .unwrap()
            .tick_strings(&["◜", "◠", "◝", "◞", "◡", "◟"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn status(label: &str, state: &str, color: &str) {
    let dots = 42usize.saturating_sub(label.len() + state.len());
    let dotstr: String = std::iter::repeat_n('·', dots).collect();
    println!("  {C_MID}│{C_RST}  {label} {C_DIM}{dotstr}{C_RST} {color}{state}{C_RST}");
}

fn ok(label: &str, state: &str) {
    status(label, state, C_OK);
}

fn fail(label: &str, state: &str) {
    status(label, state, C_ERR);
}

fn warn(label: &str, state: &str) {
    status(label, state, C_WARN);
}

fn section(title: &str) {
    println!("  {C_MID}│{C_RST}");
    println!("  {C_MID}├─ {C_HI}{C_BOLD}{title}{C_RST}");
    println!("  {C_MID}│{C_RST}");
}

// Extra greens for the tree canopy gradient
const C_G1: &str = "\x1b[38;5;22m";   // dark forest
const C_G2: &str = "\x1b[38;5;28m";   // mid forest
const C_G3: &str = "\x1b[38;5;34m";   // green
const C_G4: &str = "\x1b[38;5;40m";   // bright green
const C_G5: &str = "\x1b[38;5;46m";   // vivid green
const C_TRUNK: &str = "\x1b[38;5;94m"; // brown
const C_ROOT: &str = "\x1b[38;5;58m";  // dark brown

const C_FROG: &str = "\x1b[38;5;48m";    // bright teal-green
const C_EYES: &str = "\x1b[38;5;226m";  // yellow eyes

const C_TONGUE: &str = "\x1b[38;5;204m"; // pink tongue

fn banner() {
    println!();
    println!("       {C_G5}▄{C_RST}");
    println!("      {C_G4}▄█▄{C_RST}        {C_FROG} ▄▄▄{C_RST}");
    println!("     {C_G3}▄███▄{C_RST}     {C_FROG}▐{C_EYES}o{C_FROG} {C_EYES}o{C_FROG}▌{C_RST}");
    println!("    {C_G2}▄█████▄{C_RST}    {C_FROG}▐▄{C_TONGUE}~{C_FROG}▄▌{C_RST}");
    println!("   {C_G1}▄███████▄{C_RST}   {C_FROG}▐██▌{C_RST}");
    println!("      {C_TRUNK}▐█▌{C_RST}     {C_FROG}▗▘▝▖{C_RST}");
    println!("   {C_ROOT}▀▀▀▀█▀▀▀▀{C_RST}");
    println!();
    println!("  {C_HI}{C_BOLD}Y G G D R A S I L{C_RST} {C_DIM}v{}{C_RST}", env!("CARGO_PKG_VERSION"));
    println!();
}

/// Run `ygg init` with options.
pub async fn execute_with_options(verbose: bool, skip: &[String]) -> Result<(), anyhow::Error> {
    execute_inner(verbose, skip).await
}

pub async fn execute() -> Result<(), anyhow::Error> {
    execute_inner(false, &[]).await
}

fn should_skip(skip: &[String], name: &str) -> bool {
    skip.iter().any(|s| s.eq_ignore_ascii_case(name))
}

async fn execute_inner(_verbose: bool, skip: &[String]) -> Result<(), anyhow::Error> {
    banner();

    let pkg = detect_pkg_manager();
    let sudo = has_sudo().await;
    let pkg_name = match &pkg {
        Some(PackageManager::Brew) => "brew",
        Some(PackageManager::Apt) => "apt",
        None => "—",
    };

    // Show expected connection paths
    dotenvy::dotenv().ok();
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ygg:ygg@localhost:5432/ygg".into());
    let ollama_url = std::env::var("OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434".into());

    // Mask password in display
    let db_display = if let Some(at) = db_url.find('@') {
        if let Some(colon) = db_url[..at].rfind(':') {
            format!("{}:***@{}", &db_url[..colon], &db_url[at+1..])
        } else {
            db_url.clone()
        }
    } else {
        db_url.clone()
    };

    let embed_model = std::env::var("OLLAMA_EMBED_MODEL")
        .unwrap_or_else(|_| "all-minilm".into());
    let chat_model = std::env::var("OLLAMA_CHAT_MODEL")
        .unwrap_or_else(|_| "mistral:7b".into());
    let embed_dim = std::env::var("EMBEDDING_DIMENSIONS")
        .unwrap_or_else(|_| "384".into());

    println!("  {C_DIM}pkg{C_RST}     {pkg_name}");
    println!("  {C_DIM}sudo{C_RST}    {}", if sudo { "yes" } else { "no" });
    println!("  {C_DIM}pg{C_RST}      {db_display}");
    println!("  {C_DIM}llm{C_RST}     {ollama_url}");
    println!("  {C_DIM}chat{C_RST}    {chat_model}");
    println!("  {C_DIM}embed{C_RST}   {embed_model} {C_DIM}({embed_dim}d){C_RST}");
    println!();
    println!("  {C_MID}╭─────────────────────────────────────────────╮{C_RST}");

    section("dependencies");
    ensure_tool("tmux", &pkg, sudo).await;
    ensure_tool("jq", &pkg, sudo).await;
    check_tool_styled("rtk").await;

    section("postgresql");
    if should_skip(skip, "pg") {
        ok("postgresql", "skipped");
    } else {
        let pg_running = check_port(5432).await;
        if pg_running {
            ok("postgresql", "running");
            {
                let pb = spinner("checking pgvector...");
                install_pgvector(&pkg, sudo).await;
                pb.finish_and_clear();
                ok("pgvector", "ready");
            }
            create_database_if_needed().await;
        } else {
            match install_postgres(&pkg, sudo).await {
                Ok(()) => {
                    // Wait for it to come up
                    let pb = spinner("waiting for postgresql...");
                    let mut started = false;
                    for _ in 0..15 {
                        if check_port(5432).await { started = true; break; }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    pb.finish_and_clear();
                    if started {
                        ok("postgresql", "installed");
                        {
                            let pb = spinner("checking pgvector...");
                            install_pgvector(&pkg, sudo).await;
                            pb.finish_and_clear();
                            ok("pgvector", "ready");
                        }
                        create_database_if_needed().await;
                    } else {
                        warn("postgresql", "installed but not responding on :5432");
                        prompt_skip_or_bail("postgresql").await?;
                    }
                }
                Err(e) => {
                    warn("postgresql", "install failed");
                    println!("  {C_MID}│{C_RST}  {C_DIM}{e}{C_RST}");
                    prompt_skip_or_bail("postgresql").await?;
                }
            }
        }
    }

    section("ollama");
    if should_skip(skip, "ollama") {
        ok("ollama", "skipped");
    } else {
        let ollama_running = check_port(11434).await;
        if ollama_running {
            ok("ollama", "running");
        } else {
            match install_ollama(&pkg, sudo).await {
                Ok(()) => {
                    let pb = spinner("waiting for ollama...");
                    let mut started = false;
                    for _ in 0..15 {
                        if check_port(11434).await { started = true; break; }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    pb.finish_and_clear();
                    if started {
                        ok("ollama", "installed");
                    } else {
                        warn("ollama", "installed but not responding on :11434");
                        println!("  {C_MID}│{C_RST}  {C_DIM}try: ollama serve{C_RST}");
                        prompt_skip_or_bail("ollama").await?;
                    }
                }
                Err(e) => {
                    warn("ollama", "install failed");
                    println!("  {C_MID}│{C_RST}  {C_DIM}{e}{C_RST}");
                    prompt_skip_or_bail("ollama").await?;
                }
            }
        }
    }

    section("config");
    let env_path = Path::new(".env");
    if !env_path.exists() {
        let example = include_str!("../../.env.example");
        tokio::fs::write(env_path, example).await?;
        ok(".env", "created");
    } else {
        ok(".env", "exists");
    }

    let config = AppConfig::from_env()?;
    if !should_skip(skip, "pg") {
        let pb = spinner("running migrations...");
        let mig_result = async {
            let pool = db::create_pool(&config.database_url).await?;
            db::run_migrations(&pool).await?;
            Ok::<(), anyhow::Error>(())
        }.await;
        pb.finish_and_clear();
        match mig_result {
            Ok(()) => ok("migrations", "applied"),
            Err(e) => {
                warn("migrations", "failed");
                println!("  {C_MID}│{C_RST}  {C_DIM}{e}{C_RST}");
                prompt_skip_or_bail("migrations").await?;
            }
        }
    }

    if !should_skip(skip, "models") && check_port(11434).await {
        section("models");

        let ollama = OllamaClient::new(
            &config.ollama_base_url,
            &config.ollama_embed_model,
            &config.ollama_chat_model,
        );

        for model in [&config.ollama_embed_model, &config.ollama_chat_model] {
            let pb = spinner(&format!("pulling {model}..."));
            match ollama.pull_model(model).await {
                Result::Ok(()) => {
                    pb.finish_and_clear();
                    ok(model, "pulled");
                }
                Err(e) => {
                    pb.finish_and_clear();
                    fail(model, &format!("{e}"));
                }
            }
        }
    } else if should_skip(skip, "models") {
        section("models");
        ok("models", "skipped");
    }

    if !should_skip(skip, "statusbar") {
        section("status bar");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/vscode".into());
        let claude_dir = Path::new(&home).join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.ok();
        let script_src = include_str!("../../scripts/ygg-status.sh");
        let script_dest = claude_dir.join("ygg-status.sh");
        tokio::fs::write(&script_dest, script_src).await?;
        Command::new("chmod")
            .args(["+x", script_dest.to_str().unwrap()])
            .status()
            .await
            .ok();
        ok("ygg-status.sh", "installed");
    } else {
        section("status bar");
        ok("status bar", "skipped");
    }

    println!("  {C_MID}│{C_RST}");
    println!("  {C_MID}╰─────────────────────────────────────────────╯{C_RST}");
    println!();

    // Reset terminal state in case spinners messed it up
    print!("\x1b[?25h"); // show cursor
    let _ = std::io::Write::flush(&mut std::io::stdout());

    println!("  {C_OK}{C_BOLD}ready{C_RST}");
    println!();
    println!("  {C_DIM}skip flags: --skip pg,ollama,models,statusbar{C_RST}");
    println!();
    println!("  {C_DIM}next:{C_RST}");
    println!("    {C_HI}ygg spawn{C_RST} --task {C_DIM}\"your task\"{C_RST}");
    println!("    {C_HI}ygg dashboard{C_RST}");
    println!();

    Ok(())
}

/// Prompt user to skip or bail.
async fn prompt_skip_or_bail(name: &str) -> Result<(), anyhow::Error> {
    println!("  {C_MID}│{C_RST}");
    println!("  {C_MID}│{C_RST}  {C_WARN}skip {name} and continue? [Y/n]{C_RST}");
    if prompt_yes_no().await {
        warn(name, "skipped");
        Ok(())
    } else {
        anyhow::bail!("{name} not available. Try: ygg init --skip {name}")
    }
}

/// Check if a tool exists, install it if not.
async fn ensure_tool(name: &str, pkg: &Option<PackageManager>, sudo: bool) {
    if tool_exists(name).await {
        ok(name, "found");
        return;
    }

    let pb = spinner(&format!("installing {name}..."));
    let success = match pkg {
        Some(PackageManager::Brew) => run_cmd("brew", &["install", name]).await,
        Some(PackageManager::Apt) if sudo => {
            run_cmd("sudo", &["apt-get", "update", "-qq"]).await;
            run_cmd("sudo", &["apt-get", "install", "-y", "-qq", name]).await
        }
        Some(PackageManager::Apt) => {
            run_cmd("apt-get", &["update", "-qq"]).await;
            run_cmd("apt-get", &["install", "-y", "-qq", name]).await
        }
        None => false,
    };
    pb.finish_and_clear();

    if success && tool_exists(name).await {
        ok(name, "installed");
    } else {
        fail(name, "install manually");
    }
}

/// Just check, don't install.
async fn check_tool_styled(name: &str) {
    if tool_exists(name).await {
        ok(name, "found");
    } else {
        fail(name, "not found");
    }
}

/// Install PostgreSQL.
async fn install_postgres(pkg: &Option<PackageManager>, sudo: bool) -> Result<(), anyhow::Error> {
    match pkg {
        Some(PackageManager::Brew) => {
            // brew install can take a while — run with visible output
            let install_ok = Command::new("brew")
                .args(["install", "postgresql@16"])
                .status()
                .await
                .is_ok_and(|s| s.success());
            if !install_ok {
                anyhow::bail!("brew install postgresql@16 failed");
            }
            Command::new("brew")
                .args(["services", "start", "postgresql@16"])
                .status()
                .await
                .ok();
        }
        Some(PackageManager::Apt) => {
            if sudo {
                run_cmd("sudo", &["apt-get", "update", "-qq"]).await;
                run_cmd("sudo", &["apt-get", "install", "-y", "-qq", "postgresql", "postgresql-client"]).await;
            } else {
                run_cmd("apt-get", &["update", "-qq"]).await;
                run_cmd("apt-get", &["install", "-y", "-qq", "postgresql", "postgresql-client"]).await;
            }

            if sudo {
                if !run_cmd("sudo", &["systemctl", "start", "postgresql"]).await {
                    run_cmd("sudo", &["pg_ctlcluster", "16", "main", "start"]).await;
                }
            } else {
                let pgdata = format!(
                    "{}/pgdata",
                    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
                );
                if !Path::new(&pgdata).exists() {
                    run_cmd("initdb", &["-D", &pgdata]).await;
                }
                run_cmd("pg_ctl", &["-D", &pgdata, "-l", "/tmp/pg.log", "start"]).await;
            }

            if sudo {
                run_cmd("sudo", &["-u", "postgres", "createuser", "--superuser", "ygg"]).await;
                run_cmd("sudo", &["-u", "postgres", "psql", "-c", "ALTER USER ygg PASSWORD 'ygg';"]).await;
            } else {
                run_cmd("createuser", &["--superuser", "ygg"]).await;
                run_cmd("psql", &["-c", "ALTER USER ygg PASSWORD 'ygg';", "postgres"]).await;
            }
        }
        None => {
            anyhow::bail!("No package manager found. Install PostgreSQL manually.");
        }
    }
    Ok(())
}

/// Install pgvector extension.
async fn install_pgvector(pkg: &Option<PackageManager>, sudo: bool) {
    match pkg {
        Some(PackageManager::Brew) => {
            run_cmd("brew", &["install", "pgvector"]).await;
        }
        Some(PackageManager::Apt) => {
            let success = if sudo {
                run_cmd("sudo", &["apt-get", "install", "-y", "-qq", "postgresql-16-pgvector"]).await
                    || run_cmd("sudo", &["apt-get", "install", "-y", "-qq", "postgresql-15-pgvector"]).await
                    || run_cmd("sudo", &["apt-get", "install", "-y", "-qq", "postgresql-14-pgvector"]).await
            } else {
                run_cmd("apt-get", &["install", "-y", "-qq", "postgresql-16-pgvector"]).await
                    || run_cmd("apt-get", &["install", "-y", "-qq", "postgresql-15-pgvector"]).await
            };
            if !success {
                install_pgvector_from_source(sudo).await;
            }
        }
        None => {
            warn("pgvector", "install manually");
        }
    }
}

/// Build pgvector from source.
async fn install_pgvector_from_source(sudo: bool) {
    let tmpdir = format!("{}/pgvector-build", std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()));
    if !run_cmd("git", &["clone", "--depth", "1", "https://github.com/pgvector/pgvector.git", &tmpdir]).await {
        fail("pgvector", "clone failed");
        return;
    }

    let make_ok = Command::new("make")
        .current_dir(&tmpdir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());

    if make_ok {
        let install_ok = if sudo {
            run_cmd("sudo", &["make", "-C", &tmpdir, "install"]).await
        } else {
            Command::new("make")
                .arg("install")
                .current_dir(&tmpdir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success())
        };
        if !install_ok {
            fail("pgvector", "make install failed");
        }
    } else {
        fail("pgvector", "build failed");
    }

    let _ = std::fs::remove_dir_all(&tmpdir);
}

/// Install Ollama.
async fn install_ollama(pkg: &Option<PackageManager>, _sudo: bool) -> Result<(), anyhow::Error> {
    match pkg {
        Some(PackageManager::Brew) => {
            run_cmd("brew", &["install", "ollama"]).await;
        }
        _ => {
            let success = run_cmd("sh", &["-c", "curl -fsSL https://ollama.ai/install.sh | sh"]).await;
            if !success {
                anyhow::bail!("Ollama install failed. Install manually: https://ollama.ai");
            }
        }
    }

    Command::new("ollama")
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    tokio::time::sleep(Duration::from_secs(2)).await;

    Ok(())
}

/// Create the ygg database if it doesn't exist.
async fn create_database_if_needed() {
    if run_cmd("createdb", &["ygg"]).await {
        ok("database 'ygg'", "created");
    } else {
        ok("database 'ygg'", "exists");
    }
}

async fn tool_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn check_port(port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .is_ok()
}

/// Prompt user yes/no, default yes.
async fn prompt_yes_no() -> bool {
    use std::io::{self, BufRead, Write};
    print!("  {C_MID}│{C_RST}  > ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok();
    let answer = line.trim().to_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

async fn run_cmd(cmd: &str, args: &[&str]) -> bool {
    run_cmd_verbose(cmd, args, false).await
}

async fn run_cmd_verbose(cmd: &str, args: &[&str], verbose: bool) -> bool {
    let (stdout, stderr) = if verbose {
        (Stdio::inherit(), Stdio::inherit())
    } else {
        (Stdio::null(), Stdio::null())
    };

    match tokio::time::timeout(
        Duration::from_secs(120),
        Command::new(cmd)
            .args(args)
            .stdout(stdout)
            .stderr(stderr)
            .status(),
    )
    .await
    {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        Err(_) => {
            eprintln!("  {C_WARN}timeout{C_RST}: {cmd} {}", args.join(" "));
            false
        }
    }
}
