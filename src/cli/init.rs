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
    println!("      {G4}▄█▄{X}      {FR}  ▄▄▄▄▄{X}");
    println!("     {G3}▄███▄{X}     {FR}▐{EY}o{FR}▌▄▌{EY}o{FR}▌{X}");
    println!("    {G2}▄█████▄{X}    {FR}▐ {TG}\\/{FR} ▌{X}");
    println!("   {G1}▄███████▄{X}   {FR}▐▄▄▄▄▌{X}");
    println!("      {TK}▐█▌{X}     {FR}▐▌  ▐▌{X}");
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

fn ok(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {O}│{X}  {label} {D}{d}{X} {G}{state}{X}");
}

fn bad(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {O}│{X}  {label} {D}{d}{X} {R}{state}{X}");
}

fn hint(msg: &str) {
    println!("  {O}│{X}  {D}{msg}{X}");
}

fn head(title: &str) {
    println!("  {O}│{X}");
    println!("  {O}├─ {B}{title}{X}");
    println!("  {O}│{X}");
}

fn prompt_yes(msg: &str) -> bool {
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

async fn offer_curl_install(name: &str, script_url: &str) {
    hint(&format!("install script: {script_url}"));
    if prompt_yes(&format!("run install script for {name}?")) {
        let installed = run_show("sh", &["-c", &format!("curl -fsSL {script_url} | sh")]).await;
        if installed && has(name).await {
            ok(name, "installed");
        } else {
            bad(name, "install script failed");
            hint(&format!("try manually: curl -fsSL {script_url} | sh"));
        }
    } else if !prompt_skip(name) {
        std::process::exit(1);
    }
}

fn prompt_skip(name: &str) -> bool {
    use std::io::{self, BufRead, Write};
    println!("  {O}│{X}");
    println!("  {O}│{X}  {Y}skip {name} and continue? [Y/n]{X}");
    print!("  {O}│{X}  > ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

/// Find a binary by checking known paths, then PATH.
fn find_bin(name: &str) -> Option<String> {
    for dir in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin"] {
        let p = format!("{dir}/{name}");
        if Path::new(&p).exists() { return Some(p); }
    }
    // Check HOME-relative paths
    if let Ok(home) = std::env::var("HOME") {
        for sub in [".local/bin", ".cargo/bin"] {
            let p = format!("{home}/{sub}/{name}");
            if Path::new(&p).exists() { return Some(p); }
        }
    }
    // Fallback: which
    if let Ok(o) = std::process::Command::new("which").arg(name).stdout(Stdio::piped()).stderr(Stdio::null()).output() {
        if o.status.success() { return Some(name.to_string()); }
    }
    None
}

async fn has(name: &str) -> bool { find_bin(name).is_some() }

async fn run(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin).args(args).stdout(Stdio::null()).stderr(Stdio::null())
        .status().await.is_ok_and(|s| s.success())
}

async fn run_show(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin).args(args).status().await.is_ok_and(|s| s.success())
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

// ─── init ────────────────────────────────────────────────

pub async fn execute_with_options(_verbose: bool, skip: &[String]) -> Result<(), anyhow::Error> {
    init(skip).await
}

pub async fn execute() -> Result<(), anyhow::Error> {
    init(&[]).await
}

fn skipping(list: &[String], name: &str) -> bool {
    list.iter().any(|s| s.eq_ignore_ascii_case(name))
}

async fn init(skips: &[String]) -> Result<(), anyhow::Error> {
    // Config lives in ~/.config/ygg/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let config_dir = Path::new(&home).join(".config").join("ygg");
    tokio::fs::create_dir_all(&config_dir).await.ok();

    // Ensure we're in a valid directory — brew/apt fail if cwd is gone
    if std::env::current_dir().is_err() {
        let _ = std::env::set_current_dir(&home);
    }

    banner();

    let has_brew = has("brew").await;
    let has_apt = has("apt-get").await;
    let pkg = if has_brew { "brew" } else if has_apt { "apt" } else { "—" };

    // Load existing config if present
    let existing_env = config_dir.join(".env");
    if existing_env.exists() {
        dotenvy::from_path(&existing_env).ok();
    }
    dotenvy::dotenv().ok();

    // Detect system username for default pg connection
    let sys_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "postgres".into());

    let default_db_url = format!("postgres://localhost:5432/ygg");

    // Config file at ~/.config/ygg/.env
    let env_path = config_dir.join(".env");
    let db_url = if let Ok(url) = std::env::var("DATABASE_URL") {
        url
    } else {
        println!("  {O}│{X}  {B}PostgreSQL connection{X}");
        println!("  {O}│{X}  {D}default uses system user '{sys_user}', no password{X}");
        println!("  {O}│{X}  {D}default: {default_db_url}{X}");
        println!("  {O}│{X}");

        use std::io::{self, BufRead, Write};
        println!("  {O}│{X}  {Y}use default? [Y/n]{X}");
        print!("  {O}│{X}  > ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer).ok();
        let a = answer.trim().to_lowercase();

        if a.is_empty() || a == "y" || a == "yes" {
            default_db_url.clone()
        } else {
            println!("  {O}│{X}  {D}enter postgres URL (e.g. postgres://user:pass@host:5432/ygg){X}");
            print!("  {O}│{X}  > ");
            io::stdout().flush().ok();
            let mut url = String::new();
            io::stdin().lock().read_line(&mut url).ok();
            let url = url.trim().to_string();
            if url.is_empty() { default_db_url.clone() } else { url }
        }
    };

    let ollama_url = std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let embed_model = std::env::var("OLLAMA_EMBED_MODEL").unwrap_or_else(|_| "all-minilm".into());
    let chat_model = std::env::var("OLLAMA_CHAT_MODEL").unwrap_or_else(|_| "mistral:7b".into());
    let embed_dim = std::env::var("EMBEDDING_DIMENSIONS").unwrap_or_else(|_| "384".into());

    let db_show = db_url.find('@').and_then(|at| db_url[..at].rfind(':').map(|c| format!("{}:***@{}", &db_url[..c], &db_url[at+1..]))).unwrap_or_else(|| db_url.clone());

    println!("  {D}pkg{X}     {pkg}");
    println!("  {D}pg{X}      {db_show}");
    println!("  {D}llm{X}     {ollama_url}");
    println!("  {D}chat{X}    {chat_model}");
    println!("  {D}embed{X}   {embed_model} {D}({embed_dim}d){X}");
    println!();
    println!("  {O}╭─────────────────────────────────────────────╮{X}");

    // Collect missing deps that need manual install
    let mut missing: Vec<(&str, &str)> = Vec::new();

    // ── deps ──
    head("dependencies");

    // Tools we can brew install (no sudo needed)
    for (name, brew_pkg) in [("tmux", "tmux"), ("jq", "jq")] {
        if has(name).await {
            ok(name, "found");
        } else if has_brew {
            let pb = spin(&format!("brew install {brew_pkg}..."));
            let installed = run_show("brew", &["install", brew_pkg]).await;
            pb.finish_and_clear();
            if installed {
                ok(name, "installed");
            } else {
                bad(name, "brew install failed");
                if !prompt_skip(name) { std::process::exit(1); }
            }
        } else {
            // Can't auto-install without brew — tell user what to run
            bad(name, "not found");
            if has_apt {
                hint(&format!("run: sudo apt-get install -y {name}"));
            }
            missing.push((name, brew_pkg));
            if !prompt_skip(name) { std::process::exit(1); }
        }
    }

    // Tools with install scripts — prompt before running
    if !has("rtk").await {
        bad("rtk", "not found");
        if has_brew {
            hint("install: brew install rtk");
            if prompt_yes("install rtk via brew?") {
                let pb = spin("brew install rtk...");
                let installed = run_show("brew", &["install", "rtk"]).await;
                pb.finish_and_clear();
                if installed && has("rtk").await {
                    ok("rtk", "installed");
                } else {
                    bad("rtk", "brew install failed, trying install script...");
                    offer_curl_install("rtk", "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh").await;
                }
            } else if !prompt_skip("rtk") { std::process::exit(1); }
        } else {
            offer_curl_install("rtk", "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh").await;
        }
    } else {
        ok("rtk", "found");
    }

    // ── pg ──
    head("postgresql");

    if skipping(skips, "pg") {
        ok("postgresql", "skipped");
    } else if port_open(5432).await {
        ok("postgresql", "running");
    } else if has("psql").await {
        // Installed but not running — try to start
        ok("postgresql", "installed");
        if has_brew {
            run_show("brew", &["services", "start", "postgresql@15"]).await;
        }
        if wait_port(5432, 10).await {
            ok("postgresql", "started");
        } else {
            bad("postgresql", "not responding on :5432");
            if has_brew {
                hint("try: brew services restart postgresql@15");
            }
            if !prompt_skip("postgresql") { std::process::exit(1); }
        }
    } else if has_brew {
        // Not installed — brew install (no sudo)
        let pb = spin("brew install postgresql@15...");
        let installed = run_show("brew", &["install", "postgresql@15"]).await;
        pb.finish_and_clear();
        if installed {
            run_show("brew", &["services", "start", "postgresql@15"]).await;
            if wait_port(5432, 10).await {
                ok("postgresql", "installed");
            } else {
                bad("postgresql", "installed but won't start");
                hint("try: brew services restart postgresql@15");
                if !prompt_skip("postgresql") { std::process::exit(1); }
            }
        } else {
            bad("postgresql", "brew install failed");
            if !prompt_skip("postgresql") { std::process::exit(1); }
        }
    } else {
        // No brew — tell user what to run
        bad("postgresql", "not installed");
        if has_apt {
            hint("run: sudo apt-get install -y postgresql postgresql-client");
            hint("then: sudo systemctl start postgresql");
        } else {
            hint("install: https://www.postgresql.org/download/");
        }
        if !prompt_skip("postgresql") { std::process::exit(1); }
    }

    // pgvector + createdb (only if pg is up)
    if !skipping(skips, "pg") && port_open(5432).await {
        if has_brew {
            run("brew", &["install", "pgvector"]).await;
        }
        ok("pgvector", "ready");

        if run("createdb", &["ygg"]).await {
            ok("database 'ygg'", "created");
        } else {
            ok("database 'ygg'", "exists");
        }
    }

    // ── ollama ──
    head("ollama");

    if skipping(skips, "ollama") {
        ok("ollama", "skipped");
    } else if port_open(11434).await {
        ok("ollama", "running");
    } else if has("ollama").await {
        ok("ollama", "installed");
        Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
        if wait_port(11434, 10).await {
            ok("ollama", "started");
        } else {
            bad("ollama", "not responding on :11434");
            hint("try: ollama serve");
            if !prompt_skip("ollama") { std::process::exit(1); }
        }
    } else if has_brew {
        let pb = spin("brew install ollama...");
        let installed = run_show("brew", &["install", "ollama"]).await;
        pb.finish_and_clear();
        if installed {
            Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            if wait_port(11434, 10).await {
                ok("ollama", "installed");
            } else {
                ok("ollama", "installed");
                hint("start with: ollama serve");
            }
        } else {
            bad("ollama", "brew install failed");
            if !prompt_skip("ollama") { std::process::exit(1); }
        }
    } else {
        bad("ollama", "not installed");
        offer_curl_install("ollama", "https://ollama.com/install.sh").await;
        if has("ollama").await {
            Command::new("ollama").arg("serve").stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    // ── config ──
    head("config");

    // Write config with the user's chosen database URL
    if !env_path.exists() {
        let template = include_str!("../../.env.example");
        let env_content = template.replace(
            "postgres://localhost:5432/ygg",
            &db_url,
        );
        tokio::fs::write(&env_path, env_content).await?;
        // Re-load so config picks up the new values
        dotenvy::from_path(&env_path).ok();
        ok(&format!("{}", env_path.display()), "created");
    } else {
        ok(&format!("{}", env_path.display()), "exists");
    }

    if !skipping(skips, "pg") && port_open(5432).await {
        let pb = spin("running migrations...");
        match async {
            let pool = db::create_pool(&db_url).await?;
            db::run_migrations(&pool).await?;
            Ok::<(), anyhow::Error>(())
        }.await {
            Result::Ok(()) => { pb.finish_and_clear(); ok("migrations", "applied"); }
            Err(e) => {
                pb.finish_and_clear();
                bad("migrations", "failed");
                hint(&format!("{e}"));
                if !prompt_skip("migrations") { std::process::exit(1); }
            }
        }
    }

    // ── models ──
    if !skipping(skips, "models") && port_open(11434).await {
        head("models");
        if let Ok(cfg) = AppConfig::from_env() {
            let ollama = OllamaClient::new(&cfg.ollama_base_url, &cfg.ollama_embed_model, &cfg.ollama_chat_model);
            for model in [&cfg.ollama_embed_model, &cfg.ollama_chat_model] {
                let pb = spin(&format!("pulling {model}..."));
                match ollama.pull_model(model).await {
                    Result::Ok(()) => { pb.finish_and_clear(); ok(model, "pulled"); }
                    Err(e) => { pb.finish_and_clear(); bad(model, &format!("{e}")); }
                }
            }
        }
    }

    // ── status bar ──
    if !skipping(skips, "statusbar") {
        head("status bar");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let dest = Path::new(&home).join(".claude").join("ygg-status.sh");
        tokio::fs::create_dir_all(dest.parent().unwrap()).await.ok();
        tokio::fs::write(&dest, include_str!("../../scripts/ygg-status.sh")).await?;
        Command::new("chmod").args(["+x", dest.to_str().unwrap()]).status().await.ok();
        ok("ygg-status.sh", "installed");
    }

    // ── done ──
    println!("  {O}│{X}");
    println!("  {O}╰─────────────────────────────────────────────╯{X}");
    print!("\x1b[?25h");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Show manual install commands if anything was missing
    if !missing.is_empty() {
        println!();
        println!("  {Y}manual installs needed:{X}");
        for (name, pkg) in &missing {
            if has_apt {
                println!("    sudo apt-get install -y {pkg}");
            } else {
                println!("    # install {name} manually");
            }
        }
        println!("  {D}then re-run: ygg init{X}");
    }

    println!();
    println!("  {G}{B}ready{X}");
    println!();
    println!("  {D}next:{X}");
    println!("    {O}ygg spawn{X} --task {D}\"your task\"{X}");
    println!("    {O}ygg dashboard{X}");
    println!();

    Ok(())
}
