use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::db;

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
    println!("       {G5}▄{X}         {FR} ▄▄▄{X}");
    println!("      {G4}▄█▄{X}       {FR}▐{EY}o{FR} {EY}o{FR}▌{X}");
    println!("     {G3}▄███▄{X}      {FR} \\{TG}w{FR}/ {X}");
    println!("    {G2}▄█████▄{X}    {FR}▐▌   ▐▌{X}");
    println!("   {G1}▄███████▄{X}   {FR}▐▌   ▐▌{X}");
    println!("      {TK}▐█▌{X}      {FR}^     ^{X}");
    println!("   {RT}▀▀▀▀█▀▀▀▀{X}");
    println!();
    println!(
        "  {O}{B}Y G G D R A S I L{X} {D}v{}{X}",
        env!("CARGO_PKG_VERSION")
    );
    println!();
}

fn spin(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!("  {BR}│{X} {{spinner}} {{msg}}"))
            .unwrap()
            .tick_strings(&["◜", "◠", "◝", "◞", "◡", "◟"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

// Border color — forest green to match the tree theme
const BR: &str = "\x1b[38;5;34m";

fn ok(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {BR}│{X}  {label} {D}{d}{X} {G}{state}{X}");
}

fn bad(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {BR}│{X}  {label} {D}{d}{X} {R}{state}{X}");
}

fn hint(msg: &str) {
    println!("  {BR}│{X}  {D}{msg}{X}");
}

fn head(title: &str) {
    println!("  {BR}│{X}");
    println!("  {BR}├─ {G4}{B}{title}{X}");
    println!("  {BR}│{X}");
}

fn prompt_yes(msg: &str) -> bool {
    use std::io::{self, BufRead, Write};
    println!("  {BR}│{X}");
    println!("  {BR}│{X}  {Y}{msg} [Y/n]{X}");
    print!("  {BR}│{X}  > ");
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
    println!("  {BR}│{X}");
    println!("  {BR}│{X}  {Y}skip {name} and continue? [Y/n]{X}");
    println!("  {BR}│{X}  {D}(choice will be remembered for future runs){X}");
    print!("  {BR}│{X}  > ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();
    let skip = a.is_empty() || a == "y" || a == "yes";
    if skip {
        // Save the decision — use sync write since we're in a sync fn
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let config_dir = std::path::Path::new(&home).join(".config/ygg");
        let path = config_dir.join("skips.json");
        let mut skips: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or_default();
        let key = name.to_lowercase();
        if !skips.contains(&key) {
            skips.push(key);
            let _ = std::fs::write(
                &path,
                serde_json::to_string_pretty(&skips).unwrap_or_default(),
            );
        }
    }
    skip
}

/// Find a binary by checking known paths, then PATH.
fn find_bin(name: &str) -> Option<String> {
    for dir in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
    ] {
        let p = format!("{dir}/{name}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    // Check HOME-relative paths
    if let Ok(home) = std::env::var("HOME") {
        for sub in [".local/bin", ".cargo/bin"] {
            let p = format!("{home}/{sub}/{name}");
            if Path::new(&p).exists() {
                return Some(p);
            }
        }
    }
    // Fallback: which
    if let Ok(o) = std::process::Command::new("which")
        .arg(name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if o.status.success() {
            return Some(name.to_string());
        }
    }
    None
}

async fn has(name: &str) -> bool {
    find_bin(name).is_some()
}

async fn run(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn run_show(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin)
        .args(args)
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .is_ok()
}

async fn host_port_open(host: &str, port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("{host}:{port}"))
        .await
        .is_ok()
}

async fn wait_port(port: u16, secs: u64) -> bool {
    for _ in 0..secs {
        if port_open(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// Parse (user, host, port, password) from a postgres URL.
/// Falls back to `fallback_user` when no userinfo is present.
fn parse_pg_url_parts(url: &str, fallback_user: &str) -> (String, String, u16, Option<String>) {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(url);
    let (userinfo, hostdb) = rest.split_once('@').unwrap_or(("", rest));
    let (user, pass) = if userinfo.is_empty() {
        (fallback_user.to_string(), None)
    } else if let Some((u, p)) = userinfo.split_once(':') {
        (u.to_string(), Some(p.to_string()))
    } else {
        (userinfo.to_string(), None)
    };
    let (hostport, _) = hostdb.split_once('/').unwrap_or((hostdb, "ygg"));
    let (host, port) = if let Some((h, p)) = hostport.rsplit_once(':') {
        (h.to_string(), p.parse().unwrap_or(5432))
    } else {
        (hostport.to_string(), 5432u16)
    };
    (user, host, port, pass)
}

/// Run `createdb` against the configured postgres instance.
async fn pg_createdb(user: &str, host: &str, port: u16, pass: Option<&str>) -> bool {
    let port_s = port.to_string();
    let bin = find_bin("createdb").unwrap_or_else(|| "createdb".to_string());
    let mut cmd = Command::new(&bin);
    cmd.args(["-U", user, "-h", host, "-p", &port_s, "ygg"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = pass {
        cmd.env("PGPASSWORD", p);
    }
    cmd.status().await.is_ok_and(|s| s.success())
}

/// Run `psql -c "CREATE EXTENSION IF NOT EXISTS <ext>"` against the configured instance.
async fn pg_enable_extension(
    user: &str,
    host: &str,
    port: u16,
    pass: Option<&str>,
    ext: &str,
) -> bool {
    let port_s = port.to_string();
    let sql = format!("CREATE EXTENSION IF NOT EXISTS {ext}");
    let bin = find_bin("psql").unwrap_or_else(|| "psql".to_string());
    let mut cmd = Command::new(&bin);
    cmd.args([
        "-U", user, "-h", host, "-p", &port_s, "-d", "ygg", "-c", &sql, "-q",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if let Some(p) = pass {
        cmd.env("PGPASSWORD", p);
    }
    cmd.status().await.is_ok_and(|s| s.success())
}

/// Detect which postgresql@XX version is running via brew services or pg_config.
async fn detect_pg_version() -> String {
    // Try pg_config first
    if let Ok(output) = Command::new("pg_config")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // "PostgreSQL 16.4" → "postgresql@16"
            if let Some(major) = ver
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.split('.').next())
            {
                return format!("postgresql@{major}");
            }
        }
    }

    // Fallback: check which brew services are running
    if let Ok(output) = Command::new("brew")
        .args(["services", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if line.contains("postgresql@") && line.contains("started") {
                    if let Some(name) = line.split_whitespace().next() {
                        return name.to_string();
                    }
                }
            }
        }
    }

    // Fallback: check common install locations
    let prefixes = ["/opt/homebrew/opt", "/usr/local/opt", "/usr/lib/postgresql"];
    for prefix in prefixes {
        for ver in ["16", "15", "14"] {
            let path = format!("{prefix}/postgresql@{ver}");
            let path2 = format!("{prefix}/{ver}");
            if Path::new(&path).exists() {
                return format!("postgresql@{ver}");
            }
            if Path::new(&path2).exists() {
                return format!("postgresql@{ver}");
            }
        }
    }

    "postgresql@16".to_string()
}

/// Get the pg_config bin directory for a given postgres version.
/// Uses pg_config --bindir if available, falls back to known paths.
async fn get_pg_bin_dir(pg_version: &str) -> Option<String> {
    // Try pg_config directly
    if let Ok(output) = Command::new("pg_config")
        .arg("--bindir")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if Path::new(&dir).exists() {
                return Some(dir);
            }
        }
    }

    // Check known locations
    let candidates = [
        format!("/opt/homebrew/opt/{pg_version}/bin"),
        format!("/usr/local/opt/{pg_version}/bin"),
        format!(
            "/usr/lib/postgresql/{}/bin",
            pg_version.trim_start_matches("postgresql@")
        ),
    ];
    for dir in candidates {
        if Path::new(&dir).exists() {
            return Some(dir);
        }
    }

    None
}

/// Build pgvector from git source using the system's pg_config.
/// This bypasses brew's formula which only targets the latest 2 pg versions.
async fn build_pgvector_from_source() -> bool {
    let tmp = format!(
        "{}/ygg-pgvector-build",
        std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into())
    );

    // Clean up any previous attempt
    let _ = std::fs::remove_dir_all(&tmp);

    // Clone
    if !run_show(
        "git",
        &[
            "clone",
            "--depth",
            "1",
            "https://github.com/pgvector/pgvector.git",
            &tmp,
        ],
    )
    .await
    {
        return false;
    }

    // Build — uses pg_config from PATH (which we set earlier)
    let make_ok = Command::new("make")
        .current_dir(&tmp)
        .status()
        .await
        .is_ok_and(|s| s.success());

    if !make_ok {
        let _ = std::fs::remove_dir_all(&tmp);
        return false;
    }

    // Install — may need sudo on Linux, but on macOS with brew postgres
    // the lib dir is user-writable
    let install_ok = Command::new("make")
        .args(["install"])
        .current_dir(&tmp)
        .status()
        .await
        .is_ok_and(|s| s.success());

    let _ = std::fs::remove_dir_all(&tmp);
    install_ok
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

/// Load saved skip decisions from ~/.config/ygg/skips.json
async fn load_saved_skips(config_dir: &Path) -> Vec<String> {
    let path = config_dir.join("skips.json");
    if let Ok(data) = tokio::fs::read_to_string(&path).await {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        vec![]
    }
}

/// Save a skip decision
async fn save_skip(config_dir: &Path, name: &str) {
    let path = config_dir.join("skips.json");
    let mut skips = load_saved_skips(config_dir).await;
    let name = name.to_lowercase();
    if !skips.contains(&name) {
        skips.push(name);
    }
    let _ = tokio::fs::write(
        &path,
        serde_json::to_string_pretty(&skips).unwrap_or_default(),
    )
    .await;
}

async fn init(skips: &[String]) -> Result<(), anyhow::Error> {
    // Config lives in ~/.config/ygg/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let config_dir = Path::new(&home).join(".config").join("ygg");
    tokio::fs::create_dir_all(&config_dir).await.ok();

    // Merge CLI skips with saved skips
    let saved_skips = load_saved_skips(&config_dir).await;
    let all_skips: Vec<String> = skips
        .iter()
        .cloned()
        .chain(saved_skips.into_iter())
        .collect();

    // Ensure we're in a valid directory — brew/apt fail if cwd is gone
    if std::env::current_dir().is_err() {
        let _ = std::env::set_current_dir(&home);
    }

    banner();

    let has_brew = has("brew").await;
    let has_apt = has("apt-get").await;
    let pkg = if has_brew {
        "brew"
    } else if has_apt {
        "apt"
    } else {
        "—"
    };

    // Detect system username via whoami (most reliable)
    let sys_user = if let Ok(output) = Command::new("whoami")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "postgres".into())
        }
    } else {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "postgres".into())
    };

    let default_db_url = format!("postgres://{sys_user}@localhost:5432/ygg");
    let env_path = config_dir.join(".env");

    // Load existing config or prompt for database URL.
    let (db_url, config_changed) = if env_path.exists() {
        dotenvy::from_path(&env_path).ok();
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_db_url.clone());
        (url, false)
    } else {
        use std::io::{self, BufRead, Write};
        println!("  {BR}│{X}  {B}PostgreSQL connection{X}");
        println!("  {BR}│{X}  {D}default uses system user '{sys_user}', no password{X}");
        println!("  {BR}│{X}  {D}default: {default_db_url}{X}");
        println!("  {BR}│{X}");
        println!("  {BR}│{X}  {Y}use default? [Y/n]{X}");
        print!("  {BR}│{X}  > ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer).ok();
        let a = answer.trim().to_lowercase();

        let url = if a.is_empty() || a == "y" || a == "yes" {
            default_db_url.clone()
        } else {
            println!(
                "  {BR}│{X}  {D}enter postgres URL (e.g. postgres://user:pass@host:5432/ygg){X}"
            );
            print!("  {BR}│{X}  > ");
            io::stdout().flush().ok();
            let mut custom = String::new();
            io::stdin().lock().read_line(&mut custom).ok();
            let custom = custom.trim().to_string();
            if custom.is_empty() {
                default_db_url.clone()
            } else {
                custom
            }
        };

        // Write config immediately so it's saved
        let env_content = format!(
            "DATABASE_URL={url}\n\
             EMBEDDING_DIMENSIONS=384\n\
             CONTEXT_LIMIT_TOKENS=250000\n\
             CONTEXT_HARD_CAP_TOKENS=300000\n\
             LOCK_TTL_SECS=300\n\
             HEARTBEAT_INTERVAL_SECS=60\n\
             WATCHER_INTERVAL_SECS=30\n\
             RTK_BINARY_PATH=rtk\n\
             RUST_LOG=ygg=info\n"
        );
        let _ = tokio::fs::write(&env_path, &env_content).await;

        (url, true)
    };

    // Always force the correct DATABASE_URL in env
    unsafe {
        std::env::set_var("DATABASE_URL", &db_url);
    }

    // Parse pg connection details — used for all createdb/psql calls so we
    // connect to the right host/port/user regardless of where postgres lives.
    let (pg_user, pg_host, pg_port, pg_pass) = parse_pg_url_parts(&db_url, &sys_user);
    let pg_is_local = pg_host == "localhost" || pg_host == "127.0.0.1";

    let embed_dim = std::env::var("EMBEDDING_DIMENSIONS").unwrap_or_else(|_| "384".into());

    let db_show = db_url
        .find('@')
        .and_then(|at| {
            db_url[..at]
                .rfind(':')
                .map(|c| format!("{}:***@{}", &db_url[..c], &db_url[at + 1..]))
        })
        .unwrap_or_else(|| db_url.clone());

    println!("  {D}pkg{X}     {pkg}");
    println!("  {D}pg{X}      {db_show}");
    println!("  {D}embed{X}   all-MiniLM-L6-v2 {D}({embed_dim}d, in-process){X}");
    println!();
    println!("  {BR}╭─────────────────────────────────────────────╮{X}");

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
                if !prompt_skip(name) {
                    std::process::exit(1);
                }
            }
        } else {
            // Can't auto-install without brew — tell user what to run
            bad(name, "not found");
            if has_apt {
                hint(&format!("run: sudo apt-get install -y {name}"));
            }
            missing.push((name, brew_pkg));
            if !prompt_skip(name) {
                std::process::exit(1);
            }
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
                    offer_curl_install(
                        "rtk",
                        "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh",
                    )
                    .await;
                }
            } else if !prompt_skip("rtk") {
                std::process::exit(1);
            }
        } else {
            offer_curl_install(
                "rtk",
                "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh",
            )
            .await;
        }
    } else {
        ok("rtk", "found");
    }

    // ── pg ──
    head("postgresql");

    if skipping(&all_skips, "pg") {
        ok("postgresql", "skipped");
    } else if host_port_open(&pg_host, pg_port).await {
        ok("postgresql", "running");
    } else if pg_is_local {
        // Local postgres not responding — try to start or install it
        if has("psql").await {
            ok("postgresql", "installed");
            if has_brew {
                run_show("brew", &["services", "start", "postgresql@16"]).await;
            }
            if wait_port(5432, 10).await {
                ok("postgresql", "started");
            } else {
                bad("postgresql", "not responding on :5432");
                if has_brew {
                    hint("try: brew services restart postgresql@16");
                }
                if !prompt_skip("postgresql") {
                    std::process::exit(1);
                }
            }
        } else if has_brew {
            let pb = spin("brew install postgresql@16...");
            let installed = run_show("brew", &["install", "postgresql@16"]).await;
            pb.finish_and_clear();
            if installed {
                run_show("brew", &["services", "start", "postgresql@16"]).await;
                if wait_port(5432, 10).await {
                    ok("postgresql", "installed");
                } else {
                    bad("postgresql", "installed but won't start");
                    hint("try: brew services restart postgresql@16");
                    if !prompt_skip("postgresql") {
                        std::process::exit(1);
                    }
                }
            } else {
                bad("postgresql", "brew install failed");
                if !prompt_skip("postgresql") {
                    std::process::exit(1);
                }
            }
        } else {
            bad("postgresql", "not installed");
            if has_apt {
                hint("run: sudo apt-get install -y postgresql postgresql-client");
                hint("then: sudo systemctl start postgresql");
            } else {
                hint("install: https://www.postgresql.org/download/");
            }
            if !prompt_skip("postgresql") {
                std::process::exit(1);
            }
        }
    } else {
        // Remote postgres not reachable
        bad("postgresql", &format!("cannot reach {pg_host}:{pg_port}"));
        hint("check that your port-forward or remote host is accessible");
        if !prompt_skip("postgresql") {
            std::process::exit(1);
        }
    }

    // database + pgvector (only if pg is up)
    if !skipping(&all_skips, "pg") && host_port_open(&pg_host, pg_port).await {
        // Create database first
        if pg_createdb(&pg_user, &pg_host, pg_port, pg_pass.as_deref()).await {
            ok("database 'ygg'", "created");
        } else {
            ok("database 'ygg'", "exists");
        }

        // Now check pgvector in the ygg database
        let pgvector_ok =
            pg_enable_extension(&pg_user, &pg_host, pg_port, pg_pass.as_deref(), "vector").await;

        if pgvector_ok {
            ok("pgvector", "enabled");
        } else {
            bad("pgvector", "not available");
            if has_brew {
                // Detect which postgres version is running
                let pg_version = detect_pg_version().await;
                hint(&format!("detected postgres: {pg_version}"));

                if prompt_yes(&format!("install pgvector for {pg_version}?")) {
                    // Ensure pg_config points to the running version
                    if let Some(pg_bin) = get_pg_bin_dir(&pg_version).await {
                        hint(&format!("using pg_config from: {pg_bin}"));
                        let current_path = std::env::var("PATH").unwrap_or_default();
                        unsafe {
                            std::env::set_var("PATH", format!("{pg_bin}:{current_path}"));
                        }
                    }

                    // Build pgvector from source directly — brew formula
                    // only targets the latest 2 pg versions
                    let pb = spin("building pgvector from source...");
                    let ok_built = build_pgvector_from_source().await;
                    pb.finish_and_clear();

                    if ok_built {
                        run_show("brew", &["services", "restart", &pg_version]).await;
                        wait_port(5432, 10).await;

                        if pg_enable_extension(
                            &pg_user,
                            &pg_host,
                            pg_port,
                            pg_pass.as_deref(),
                            "vector",
                        )
                        .await
                        {
                            ok("pgvector", "built + enabled");
                        } else {
                            bad("pgvector", "built but can't enable");
                            hint("check: psql -d ygg -c \"CREATE EXTENSION vector\" 2>&1");
                            if !prompt_skip("pgvector") {
                                std::process::exit(1);
                            }
                        }
                    } else {
                        bad("pgvector", "build failed");
                        hint("requires: make, gcc/clang, postgresql server dev headers");
                        if !prompt_skip("pgvector") {
                            std::process::exit(1);
                        }
                    }
                } else if !prompt_skip("pgvector") {
                    std::process::exit(1);
                }
            } else if has_apt {
                hint("run: sudo apt-get install -y postgresql-16-pgvector");
                hint("then: psql -d ygg -c 'CREATE EXTENSION vector'");
                if !prompt_skip("pgvector") {
                    std::process::exit(1);
                }
            } else {
                hint("install: https://github.com/pgvector/pgvector");
                if !prompt_skip("pgvector") {
                    std::process::exit(1);
                }
            }
        }
    }

    // ── ollama (for embeddings) ──
    head("ollama");

    if skipping(&all_skips, "ollama") {
        ok("ollama", "skipped");
    } else if port_open(11434).await {
        ok("ollama", "running");

        // Pull embed model
        let embedder = crate::embed::Embedder::default_ollama();
        let pb = spin("pulling all-minilm embedding model...");
        match embedder.pull_model().await {
            Ok(()) => {
                pb.finish_and_clear();
                ok("all-minilm", "pulled");
            }
            Err(e) => {
                pb.finish_and_clear();
                bad("all-minilm", &format!("{e}"));
            }
        }

        // Smoke test
        let pb = spin("testing embedding...");
        match embedder.embed("hello world").await {
            Ok(_) => {
                pb.finish_and_clear();
                ok("embedding", "ok (384d)");
            }
            Err(e) => {
                pb.finish_and_clear();
                bad("embedding", &format!("{e}"));
            }
        }
    } else if has("ollama").await {
        ok("ollama", "installed");
        hint("starting ollama...");
        Command::new("ollama")
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        if wait_port(11434, 10).await {
            ok("ollama", "started");
        } else {
            bad("ollama", "didn't start");
            if !prompt_skip("ollama") {
                std::process::exit(1);
            }
        }
    } else {
        bad("ollama", "not found");
        if has_brew {
            if prompt_yes("install ollama via brew?") {
                let pb = spin("brew install ollama...");
                let installed = run_show("brew", &["install", "ollama"]).await;
                pb.finish_and_clear();
                if installed {
                    ok("ollama", "installed");
                    Command::new("ollama")
                        .arg("serve")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .ok();
                    wait_port(11434, 10).await;
                } else {
                    bad("ollama", "brew install failed");
                }
            }
        } else {
            offer_curl_install("ollama", "https://ollama.com/install.sh").await;
        }
        if !port_open(11434).await && !prompt_skip("ollama") {
            std::process::exit(1);
        }
    }

    // ── config ──
    head("config");

    if config_changed {
        ok(&format!("{}", env_path.display()), "created");
    } else {
        ok(&format!("{}", env_path.display()), "exists");
    }

    if !skipping(&all_skips, "pg") && host_port_open(&pg_host, pg_port).await {
        // Ensure database exists
        pg_createdb(&pg_user, &pg_host, pg_port, pg_pass.as_deref()).await;

        let pb = spin("running migrations...");
        match async {
            let pool = db::create_pool(&db_url).await?;
            db::run_migrations(&pool).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        {
            Result::Ok(()) => {
                pb.finish_and_clear();
                ok("migrations", "applied");
            }
            Err(e) => {
                pb.finish_and_clear();
                let err_str = format!("{e}");
                bad("migrations", "failed");

                if err_str.contains("role") && err_str.contains("does not exist") {
                    hint("the configured role doesn't exist on this postgres server");
                    hint(&format!("current URL: {db_url}"));
                    hint("");
                    // Offer to reconfigure the URL in-place
                    if prompt_yes("reconfigure the database URL now?") {
                        use std::io::{self, BufRead, Write};
                        hint(&format!("your system user is: {sys_user}"));
                        println!(
                            "  {BR}│{X}  {D}enter postgres URL (e.g. postgres://user:pass@host:port/ygg){X}"
                        );
                        print!("  {BR}│{X}  > ");
                        io::stdout().flush().ok();
                        let mut new_url = String::new();
                        io::stdin().lock().read_line(&mut new_url).ok();
                        let new_url = new_url.trim().to_string();
                        if !new_url.is_empty() {
                            let new_content = format!(
                                "DATABASE_URL={new_url}\n\
                                 EMBEDDING_DIMENSIONS=384\n\
                                 CONTEXT_LIMIT_TOKENS=250000\n\
                                 CONTEXT_HARD_CAP_TOKENS=300000\n\
                                 LOCK_TTL_SECS=300\n\
                                 HEARTBEAT_INTERVAL_SECS=60\n\
                                 WATCHER_INTERVAL_SECS=30\n\
                                 RTK_BINARY_PATH=rtk\n\
                                 RUST_LOG=ygg=info\n"
                            );
                            if tokio::fs::write(&env_path, &new_content).await.is_ok() {
                                ok("config", "updated — re-run: ygg init");
                            }
                        }
                    } else {
                        hint(&format!("  rm {}", env_path.display()));
                        hint("  ygg init");
                    }
                } else if err_str.contains("does not exist") && err_str.contains("database") {
                    hint("database 'ygg' doesn't exist");
                    hint(&format!("  createdb -U {sys_user} ygg"));
                } else {
                    hint(&format!("{e}"));
                }

                if !prompt_skip("migrations") {
                    std::process::exit(1);
                }
            }
        }
    }

    // No Ollama model pulls needed — embedding is in-process via fastembed

    // ── hooks + status bar ──
    if !skipping(&all_skips, "hooks") {
        head("hooks");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let claude_dir = Path::new(&home).join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.ok();

        // Install status bar script
        let status_dest = claude_dir.join("ygg-status.sh");
        tokio::fs::write(&status_dest, include_str!("../../scripts/ygg-status.sh")).await?;
        Command::new("chmod")
            .args(["+x", status_dest.to_str().unwrap()])
            .status()
            .await
            .ok();
        ok("ygg-status.sh", "installed");

        // Install hook scripts
        let hooks_dir = claude_dir.join("ygg-hooks");
        tokio::fs::create_dir_all(&hooks_dir).await.ok();

        for (name, content) in [
            (
                "session-start.sh",
                include_str!("../../scripts/hooks/session-start.sh"),
            ),
            (
                "pre-tool-use.sh",
                include_str!("../../scripts/hooks/pre-tool-use.sh"),
            ),
            (
                "prompt-submit.sh",
                include_str!("../../scripts/hooks/prompt-submit.sh"),
            ),
            (
                "pre-compact.sh",
                include_str!("../../scripts/hooks/pre-compact.sh"),
            ),
            ("stop.sh", include_str!("../../scripts/hooks/stop.sh")),
        ] {
            let dest = hooks_dir.join(name);
            tokio::fs::write(&dest, content).await?;
            Command::new("chmod")
                .args(["+x", dest.to_str().unwrap()])
                .status()
                .await
                .ok();
        }
        ok("hook scripts", "installed");

        // Install slash commands into ~/.claude/commands/
        let commands_dir = claude_dir.join("commands");
        tokio::fs::create_dir_all(&commands_dir).await.ok();
        for (name, content) in [
            (
                "ygg-status.md",
                include_str!("../../scripts/commands/ygg-status.md"),
            ),
            (
                "ygg-spawn.md",
                include_str!("../../scripts/commands/ygg-spawn.md"),
            ),
            (
                "ygg-lock.md",
                include_str!("../../scripts/commands/ygg-lock.md"),
            ),
        ] {
            let dest = commands_dir.join(name);
            tokio::fs::write(&dest, content).await?;
        }
        ok("slash commands", "installed");

        // Install hooks into Claude Code settings
        let settings_path = claude_dir.join("settings.json");
        let hooks_path = hooks_dir.to_string_lossy();

        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{hooks_path}/session-start.sh")}]}],
                "PreToolUse": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{hooks_path}/pre-tool-use.sh")}]}],
                "UserPromptSubmit": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{hooks_path}/prompt-submit.sh")}]}],
                "PreCompact": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{hooks_path}/pre-compact.sh")}]}],
                "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{hooks_path}/stop.sh")}]}]
            },
            "statusLine": {
                "type": "command",
                "command": format!("{}", status_dest.to_string_lossy()),
                "refreshInterval": 3
            }
        });

        // Merge with existing settings if present
        let final_settings = if settings_path.exists() {
            let existing = tokio::fs::read_to_string(&settings_path)
                .await
                .unwrap_or_default();
            if let Ok(mut existing_json) = serde_json::from_str::<serde_json::Value>(&existing) {
                // Merge hooks and statusLine into existing
                if let Some(obj) = existing_json.as_object_mut() {
                    obj.insert("hooks".to_string(), settings["hooks"].clone());
                    obj.insert("statusLine".to_string(), settings["statusLine"].clone());
                }
                existing_json
            } else {
                settings
            }
        } else {
            settings
        };

        tokio::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&final_settings)?,
        )
        .await?;
        ok("claude settings.json", "hooks registered");

        hint("hooks will fire automatically in Claude Code sessions");
        hint("no manual ygg commands needed — just use Claude normally");
    }

    // ── project integration ──
    if !skipping(&all_skips, "project") {
        if let Ok(cwd) = std::env::current_dir() {
            head("project integration");
            if super::init_project::has_any_content(&cwd) {
                hint("CLAUDE.md or AGENTS.md already has content — skipping auto-install");
                hint("run `ygg integrate` to install the managed block, `--remove` to strip it");
            } else {
                match super::init_project::install(&cwd) {
                    Ok(report) => {
                        for (path, action) in &report.files {
                            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                            ok(name, &action.to_string());
                        }
                    }
                    Err(e) => {
                        bad("project integration", &format!("{e}"));
                    }
                }
            }
        }
    }

    // ── done ──
    println!("  {BR}│{X}");
    println!("  {BR}╰─────────────────────────────────────────────╯{X}");
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

#[cfg(test)]
mod tests {
    /// yggdrasil-107: hook drift regression. The PreToolUse hook MUST bump
    /// `task_runs.heartbeat_at` so the scheduler doesn't reap live agents as
    /// crashed (yggdrasil-99). Drop this line from scripts/hooks and the
    /// scheduler stops working as soon as a user runs `ygg init`.
    #[test]
    fn pre_tool_use_hook_includes_heartbeat() {
        let content = include_str!("../../scripts/hooks/pre-tool-use.sh");
        assert!(
            content.contains("ygg run heartbeat"),
            "pre-tool-use.sh must invoke `ygg run heartbeat` (see yggdrasil-99); installed hooks come from this file"
        );
    }

    /// yggdrasil-107: same drift class. Stop hook owns the run-terminal
    /// transition + commit/branch capture (yggdrasil-97). If this line goes
    /// missing, runs never finalize and the scheduler reaper has to clean up
    /// (slower, lossier).
    #[test]
    fn stop_hook_includes_capture_outcome() {
        let content = include_str!("../../scripts/hooks/stop.sh");
        assert!(
            content.contains("ygg run capture-outcome"),
            "stop.sh must invoke `ygg run capture-outcome` (see yggdrasil-97)"
        );
    }
}
