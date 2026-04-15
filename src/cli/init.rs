use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::process::Command;

use crate::config::AppConfig;
use crate::db;
use crate::ollama::OllamaClient;

// Colors
const D: &str = "\x1b[90m";
const O: &str = "\x1b[38;5;208m";
const G: &str = "\x1b[38;5;114m";
const R: &str = "\x1b[38;5;203m";
const Y: &str = "\x1b[38;5;221m";
const B: &str = "\x1b[1m";
const X: &str = "\x1b[0m";

// Tree colors
const G1: &str = "\x1b[38;5;22m";
const G2: &str = "\x1b[38;5;28m";
const G3: &str = "\x1b[38;5;34m";
const G4: &str = "\x1b[38;5;40m";
const G5: &str = "\x1b[38;5;46m";
const TK: &str = "\x1b[38;5;94m";
const RT: &str = "\x1b[38;5;58m";
const FR: &str = "\x1b[38;5;48m";
const EY: &str = "\x1b[38;5;226m";
const TG: &str = "\x1b[38;5;204m";

fn banner() {
    println!();
    println!("       {G5}▄{X}");
    println!("      {G4}▄█▄{X}        {FR} ▄▄▄{X}");
    println!("     {G3}▄███▄{X}     {FR}▐{EY}o{FR} {EY}o{FR}▌{X}");
    println!("    {G2}▄█████▄{X}    {FR}▐▄{TG}~{FR}▄▌{X}");
    println!("   {G1}▄███████▄{X}   {FR}▐██▌{X}");
    println!("      {TK}▐█▌{X}     {FR}▗▘▝▖{X}");
    println!("   {RT}▀▀▀▀█▀▀▀▀{X}");
    println!();
    println!("  {O}{B}Y G G D R A S I L{X} {D}v{}{X}", env!("CARGO_PKG_VERSION"));
    println!();
}

fn spin(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!("  {O}│{X} {{spinner}} {{msg}}"))
            .unwrap()
            .tick_strings(&["◜", "◠", "◝", "◞", "◡", "◟"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn line(label: &str, state: &str, color: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {O}│{X}  {label} {D}{d}{X} {color}{state}{X}");
}

fn head(title: &str) {
    println!("  {O}│{X}");
    println!("  {O}├─ {B}{title}{X}");
    println!("  {O}│{X}");
}

fn prompt(msg: &str) -> bool {
    use std::io::{self, BufRead, Write};
    println!("  {O}│{X}");
    println!("  {O}│{X}  {Y}{msg} [Y/n]{X}");
    print!("  {O}│{X}  > ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

/// Find a binary by checking known paths, then falling back to which.
fn find_bin(name: &str) -> Option<String> {
    let known = ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin"];
    for dir in known {
        let p = format!("{dir}/{name}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    // Fallback: check PATH
    if std::process::Command::new("which").arg(name).stdout(Stdio::piped()).stderr(Stdio::null()).output()
        .is_ok_and(|o| o.status.success()) {
        return Some(name.to_string());
    }
    None
}

async fn has(name: &str) -> bool {
    find_bin(name).is_some()
}

async fn run(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin).args(args).stdout(Stdio::null()).stderr(Stdio::null())
        .status().await.is_ok_and(|s| s.success())
}

async fn run_visible(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin).args(args)
        .status().await.is_ok_and(|s| s.success())
}

async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.is_ok()
}

async fn wait_port(port: u16, secs: u64) -> bool {
    for _ in 0..secs {
        if port_open(port).await { return true; }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

fn is_mac() -> bool {
    cfg!(target_os = "macos")
}

// ─── init ────────────────────────────────────────────────

pub async fn execute_with_options(_verbose: bool, skip: &[String]) -> Result<(), anyhow::Error> {
    execute_inner(skip).await
}

pub async fn execute() -> Result<(), anyhow::Error> {
    execute_inner(&[]).await
}

fn skip(list: &[String], name: &str) -> bool {
    list.iter().any(|s| s.eq_ignore_ascii_case(name))
}

async fn execute_inner(skips: &[String]) -> Result<(), anyhow::Error> {
    banner();

    let has_brew = has("brew").await;
    let has_apt = has("apt-get").await;
    let pkg = if has_brew { "brew" } else if has_apt { "apt" } else { "—" };

    dotenvy::dotenv().ok();
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://ygg:ygg@localhost:5432/ygg".into());
    let ollama_url = std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let embed_model = std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| "all-minilm".into());
    let chat_model = std::env::var("OLLAMA_CHAT_MODEL").unwrap_or_else(|_| "mistral:7b".into());
    let embed_dim = std::env::var("EMBEDDING_DIMENSIONS").unwrap_or_else(|_| "384".into());

    // Mask password
    let db_show = db_url.find('@').and_then(|at| db_url[..at].rfind(':').map(|c| format!("{}:***@{}", &db_url[..c], &db_url[at+1..]))).unwrap_or_else(|| db_url.clone());

    println!("  {D}pkg{X}     {pkg}");
    println!("  {D}pg{X}      {db_show}");
    println!("  {D}llm{X}     {ollama_url}");
    println!("  {D}chat{X}    {chat_model}");
    println!("  {D}embed{X}   {embed_model} {D}({embed_dim}d){X}");
    println!();
    println!("  {O}╭─────────────────────────────────────────────╮{X}");

    // ── deps ──
    head("dependencies");

    for (name, hint) in [("tmux", "brew install tmux"), ("jq", "brew install jq"), ("rtk", "https://github.com/rtk-ai/rtk")] {
        if has(name).await {
            line(name, "found", G);
        } else {
            line(name, "not found", R);
            println!("  {O}│{X}  {D}install: {hint}{X}");
            if !prompt(&format!("skip {name} and continue?")) {
                std::process::exit(1);
            }
        }
    }

    // ── pg ──
    head("postgresql");

    if skip(skips, "pg") {
        line("postgresql", "skipped", D);
    } else if port_open(5432).await {
        line("postgresql", "running", G);
    } else if has("psql").await {
        line("postgresql", "installed, starting...", Y);
        if has_brew {
            run_visible("brew", &["services", "start", "postgresql@16"]).await;
        }
        if !wait_port(5432, 10).await {
            line("postgresql", "not responding on :5432", R);
            if !prompt("skip postgresql and continue?") { std::process::exit(1); }
        } else {
            line("postgresql", "running", G);
        }
    } else {
        // Need to install
        if has_brew {
            let pb = spin("brew install postgresql@16...");
            let ok = run_visible("brew", &["install", "postgresql@16"]).await;
            pb.finish_and_clear();
            if ok {
                run_visible("brew", &["services", "start", "postgresql@16"]).await;
                if wait_port(5432, 10).await {
                    line("postgresql", "installed", G);
                } else {
                    line("postgresql", "installed but not responding", Y);
                    if !prompt("skip and continue?") { std::process::exit(1); }
                }
            } else {
                line("postgresql", "brew install failed", R);
                if !prompt("skip and continue?") { std::process::exit(1); }
            }
        } else if has_apt {
            run("apt-get", &["update", "-qq"]).await;
            run("apt-get", &["install", "-y", "-qq", "postgresql", "postgresql-client"]).await;
            if wait_port(5432, 10).await {
                line("postgresql", "installed", G);
            } else {
                line("postgresql", "install may have failed", R);
                if !prompt("skip and continue?") { std::process::exit(1); }
            }
        } else {
            line("postgresql", "no package manager", R);
            if !prompt("skip and continue?") { std::process::exit(1); }
        }
    }

    // pgvector + createdb (only if pg is available)
    if !skip(skips, "pg") && port_open(5432).await {
        if has_brew && !run("psql", &["-c", "SELECT 1 FROM pg_available_extensions WHERE name='vector'", "postgres"]).await {
            run("brew", &["install", "pgvector"]).await;
        }
        line("pgvector", "ready", G);

        // Ensure database exists
        if run("createdb", &["ygg"]).await {
            line("database 'ygg'", "created", G);
        } else {
            line("database 'ygg'", "exists", G);
        }
    }

    // ── ollama ──
    head("ollama");

    if skip(skips, "ollama") {
        line("ollama", "skipped", D);
    } else if port_open(11434).await {
        line("ollama", "running", G);
    } else if has("ollama").await {
        line("ollama", "installed, starting...", Y);
        Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
        if wait_port(11434, 10).await {
            line("ollama", "running", G);
        } else {
            line("ollama", "not responding", R);
            println!("  {O}│{X}  {D}try: ollama serve{X}");
            if !prompt("skip and continue?") { std::process::exit(1); }
        }
    } else {
        if has_brew {
            let pb = spin("brew install ollama...");
            let ok = run_visible("brew", &["install", "ollama"]).await;
            pb.finish_and_clear();
            if ok {
                Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
                if wait_port(11434, 10).await {
                    line("ollama", "installed", G);
                } else {
                    line("ollama", "installed, run: ollama serve", Y);
                }
            } else {
                line("ollama", "brew install failed", R);
                if !prompt("skip and continue?") { std::process::exit(1); }
            }
        } else {
            let pb = spin("installing ollama...");
            let ok = run("sh", &["-c", "curl -fsSL https://ollama.ai/install.sh | sh"]).await;
            pb.finish_and_clear();
            if ok {
                Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
                if wait_port(11434, 10).await {
                    line("ollama", "installed", G);
                } else {
                    line("ollama", "installed, run: ollama serve", Y);
                }
            } else {
                line("ollama", "install failed", R);
                println!("  {O}│{X}  {D}https://ollama.ai{X}");
                if !prompt("skip and continue?") { std::process::exit(1); }
            }
        }
    }

    // ── config ──
    head("config");

    let env_path = Path::new(".env");
    if !env_path.exists() {
        tokio::fs::write(env_path, include_str!("../../.env.example")).await?;
        line(".env", "created", G);
    } else {
        line(".env", "exists", G);
    }

    // migrations
    if !skip(skips, "pg") && port_open(5432).await {
        let pb = spin("running migrations...");
        match async {
            let pool = db::create_pool(&db_url).await?;
            db::run_migrations(&pool).await?;
            Ok::<(), anyhow::Error>(())
        }.await {
            Ok(()) => { pb.finish_and_clear(); line("migrations", "applied", G); }
            Err(e) => {
                pb.finish_and_clear();
                line("migrations", "failed", R);
                println!("  {O}│{X}  {D}{e}{X}");
                if !prompt("skip and continue?") { std::process::exit(1); }
            }
        }
    }

    // ── models ──
    if !skip(skips, "models") && port_open(11434).await {
        head("models");
        let config = AppConfig::from_env().ok();
        if let Some(cfg) = config {
            let ollama = OllamaClient::new(&cfg.ollama_base_url, &cfg.ollama_embed_model, &cfg.ollama_chat_model);
            for model in [&cfg.ollama_embed_model, &cfg.ollama_chat_model] {
                let pb = spin(&format!("pulling {model}..."));
                match ollama.pull_model(model).await {
                    Ok(()) => { pb.finish_and_clear(); line(model, "pulled", G); }
                    Err(e) => { pb.finish_and_clear(); line(model, &format!("{e}"), R); }
                }
            }
        }
    }

    // ── status bar ──
    if !skip(skips, "statusbar") {
        head("status bar");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dest = Path::new(&home).join(".claude").join("ygg-status.sh");
        tokio::fs::create_dir_all(dest.parent().unwrap()).await.ok();
        tokio::fs::write(&dest, include_str!("../../scripts/ygg-status.sh")).await?;
        Command::new("chmod").args(["+x", dest.to_str().unwrap()]).status().await.ok();
        line("ygg-status.sh", "installed", G);
    }

    // ── done ──
    println!("  {O}│{X}");
    println!("  {O}╰─────────────────────────────────────────────╯{X}");

    // Restore cursor
    print!("\x1b[?25h");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    println!();
    println!("  {G}{B}ready{X}");
    println!();
    println!("  {D}next:{X}");
    println!("    {O}ygg spawn{X} --task {D}\"your task\"{X}");
    println!("    {O}ygg dashboard{X}");
    println!();

    Ok(())
}
