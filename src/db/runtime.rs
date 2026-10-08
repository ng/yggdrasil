//! Managed process prototype. Installation and application-role provisioning are
//! separate; callers supply an explicitly selected native distribution. Dropping
//! a client or owner never stops PostgreSQL or removes persistent data.
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection, postgres::PgConnectOptions};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};
use uuid::Uuid;

const BOOTSTRAP: &str = "ygg_bootstrap";
const LIMIT: u64 = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    root: PathBuf,
    cluster_id: Uuid,
    system_id: String,
    major: u32,
    binary_version: String,
    bin: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ManagedCluster {
    root: PathBuf,
    manifest: Manifest,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Ready {
        pid: i32,
    },
    /// Existing process/socket cannot yet be proven to be this cluster. Never
    /// signal it, remove its PID file, or start a competing server.
    Unverified,
}

pub struct Owner {
    cluster: ManagedCluster,
    _lease: File,
    child: Option<Child>,
}

fn private_dir(path: &Path, create: bool) -> Result<()> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "managed directory must be a private owned directory: {}",
        path.display()
    );
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "expected regular managed file");
    let mut text = String::new();
    (&mut file).take(LIMIT + 1).read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= LIMIT, "managed metadata exceeds limit");
    Ok(text)
}

fn lease(root: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(".owner.lock"))?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "invalid managed ownership lock"
    );
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn command(executable: &Path) -> Command {
    let mut cmd = Command::new(executable);
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

async fn output(cmd: &mut Command, duration: Duration) -> Result<String> {
    let output = timeout(duration, cmd.output())
        .await
        .context("managed command timed out")??;
    ensure!(
        output.status.success(),
        "managed command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

impl ManagedCluster {
    /// Explicit bootstrap only. Never invoked by status, hooks or open. Interrupted
    /// initialization is retained for inspection; it is never silently reset.
    pub async fn initialize(root: &Path, bin: &Path, major: u32) -> Result<Self> {
        ensure!(
            root.is_absolute() && bin.is_absolute(),
            "absolute managed paths required"
        );
        private_dir(root, true)?;
        let root = root.canonicalize()?;
        let _lease = lease(&root)?.context("managed cluster is owned by another process")?;
        ensure!(
            fs::read_dir(&root)?.all(|entry| entry.is_ok_and(|e| e.file_name() == ".owner.lock")),
            "managed root is not empty; refusing initialization"
        );
        let bin = bin.canonicalize()?;
        let binary_version = output(
            command(&bin.join("postgres")).arg("--version"),
            Duration::from_secs(5),
        )
        .await?;
        ensure!(
            binary_version
                .split_whitespace()
                .nth(2)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
                == Some(major),
            "selected PostgreSQL binary major differs from requested major"
        );
        let socket = root.join("runtime");
        // Portable sockaddr_un budget, including /.s.PGSQL.5432 and NUL.
        ensure!(
            socket.as_os_str().as_encoded_bytes().len() + 15 < 104,
            "managed socket path too long"
        );
        private_dir(&socket, true)?;
        private_dir(&root.join("logs"), true)?;
        let data = root.join("data");
        output(
            command(&bin.join("initdb")).arg("-D").arg(&data).args([
                "--username",
                BOOTSTRAP,
                "--auth-local=trust",
                "--auth-host=reject",
                "--encoding=UTF8",
                "--locale=C",
            ]),
            Duration::from_secs(60),
        )
        .await?;
        private_dir(&data, false)?;
        let control = output(
            command(&bin.join("pg_controldata")).arg(&data),
            Duration::from_secs(5),
        )
        .await?;
        let system_id = control
            .lines()
            .find_map(|line| line.strip_prefix("Database system identifier:"))
            .context("cluster system identifier missing")?
            .trim()
            .to_owned();
        ensure!(
            system_id.parse::<u64>().is_ok(),
            "invalid cluster system identifier"
        );
        let mut config = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(data.join("postgresql.conf"))?;
        let socket = socket
            .to_str()
            .context("managed socket path must be UTF-8")?
            .replace('\\', "\\\\")
            .replace('\'', "''");
        writeln!(
            config,
            "\n# Yggdrasil managed runtime\nlisten_addresses = ''\nport = 5432\nunix_socket_directories = '{socket}'\nunix_socket_permissions = 0700\nfsync = on\nfull_page_writes = on"
        )?;
        config.sync_all()?;
        let manifest = Manifest {
            version: 1,
            root: root.clone(),
            cluster_id: Uuid::new_v4(),
            system_id,
            major,
            binary_version,
            bin,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(".cluster.tmp"))?;
        file.write_all(&serde_json::to_vec(&manifest)?)?;
        file.sync_all()?;
        fs::rename(root.join(".cluster.tmp"), root.join("cluster.json"))?;
        File::open(&root)?.sync_all()?;
        Ok(Self { root, manifest })
    }

    /// Inspect existing metadata only. Does not create directories or start a server.
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root, false)?;
        let root = root.canonicalize()?;
        let manifest: Manifest = serde_json::from_str(&read(&root.join("cluster.json"))?)?;
        ensure!(
            manifest.version == 1
                && manifest.root == root
                && manifest.bin.is_absolute()
                && manifest.system_id.parse::<u64>().is_ok(),
            "invalid cluster manifest or relocated cluster; explicit move required"
        );
        ensure!(
            root.join("runtime").to_str().is_some(),
            "managed socket path must be UTF-8"
        );
        for dir in ["data", "runtime", "logs"] {
            private_dir(&root.join(dir), false)?;
        }
        ensure!(
            read(&root.join("data/PG_VERSION"))?.trim().parse::<u32>()? == manifest.major,
            "cluster major differs from manifest; explicit upgrade required"
        );
        Ok(Self { root, manifest })
    }

    pub fn id(&self) -> Uuid {
        self.manifest.cluster_id
    }

    /// Administrative connection for lifecycle verification only. Application
    /// connections must use the separately provisioned limited runtime role.
    fn admin_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(self.root.join("runtime").to_str().unwrap())
            .port(5432)
            .username(BOOTSTRAP)
            .database("postgres")
            .application_name("ygg-managed-supervisor")
    }

    async fn verified_pid(&self) -> Result<i32> {
        let before = read(&self.root.join("data/postmaster.pid"))?;
        let lines: Vec<_> = before.lines().collect();
        ensure!(lines.len() >= 8, "incomplete server PID file");
        let pid: i32 = lines[0].parse()?;
        let start: i64 = lines[2].parse()?;
        ensure!(
            pid > 1
                && Path::new(lines[1]) == self.root.join("data")
                && lines[3] == "5432"
                && Path::new(lines[4]) == self.root.join("runtime"),
            "server PID identity mismatch"
        );
        let mut conn = PgConnection::connect_with(&self.admin_options()).await?;
        let (directory, system_id, version, listen, started, backend): (String, String, i32, String, i64, i32) = sqlx::query_as(
            "SELECT current_setting('data_directory'), system_identifier::text, current_setting('server_version_num')::int, current_setting('listen_addresses'), floor(extract(epoch FROM pg_postmaster_start_time()))::bigint, pg_backend_pid() FROM pg_control_system()")
            .fetch_one(&mut conn).await?;
        ensure!(
            Path::new(&directory) == self.root.join("data")
                && system_id == self.manifest.system_id
                && version / 10000 == self.manifest.major as i32
                && listen.is_empty()
                && started == start,
            "live server identity mismatch"
        );
        // Bind the PID file to the actual parent of our authenticated backend,
        // not merely to an unrelated process currently using a recycled PID.
        let parent = output(
            command(Path::new("/bin/ps")).args(["-p", &backend.to_string(), "-o", "ppid="]),
            Duration::from_secs(2),
        )
        .await?;
        ensure!(
            parent.trim().parse::<i32>()? == pid,
            "live postmaster PID mismatch"
        );
        ensure!(
            read(&self.root.join("data/postmaster.pid"))? == before,
            "server changed during verification"
        );
        conn.close().await?;
        Ok(pid)
    }

    pub async fn status(&self) -> Result<Status> {
        let pid_file = self.root.join("data/postmaster.pid");
        match fs::symlink_metadata(&pid_file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Stopped),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        match timeout(Duration::from_secs(3), self.verified_pid()).await {
            Ok(Ok(pid)) => Ok(Status::Ready { pid }),
            _ => {
                // A crashed postmaster can leave a PID file. Let PostgreSQL
                // perform its own stale-lock recovery only when the recorded
                // positive PID demonstrably does not exist. Never unlink it.
                let text = read(&pid_file)?;
                let lines: Vec<_> = text.lines().collect();
                if lines.len() >= 2 && Path::new(lines[1]) == self.root.join("data") {
                    if let Ok(pid) = lines[0].parse::<i32>() {
                        if pid > 1
                            && unsafe { libc::kill(pid, 0) } == -1
                            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                        {
                            return Ok(Status::Stopped);
                        }
                    }
                }
                Ok(Status::Unverified)
            }
        }
    }

    pub fn try_owner(&self) -> Result<Option<Owner>> {
        Ok(lease(&self.root)?.map(|lease| Owner {
            cluster: self.clone(),
            _lease: lease,
            child: None,
        }))
    }

    pub async fn wait_ready(&self, duration: Duration) -> Result<i32> {
        timeout(duration, async {
            loop {
                if let Status::Ready { pid } = self.status().await? {
                    return Ok(pid);
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("managed readiness timed out; server state retained")?
    }
}

impl Owner {
    pub async fn start_or_adopt(&mut self, duration: Duration) -> Result<i32> {
        timeout(duration, self.start_or_adopt_inner(duration))
            .await
            .context("managed startup timed out; server state retained")?
    }

    async fn start_or_adopt_inner(&mut self, duration: Duration) -> Result<i32> {
        loop {
            if let Some(child) = &mut self.child {
                if child.try_wait()?.is_some() {
                    self.child = None;
                }
            }
            match self.cluster.status().await? {
                Status::Ready { pid } => return Ok(pid),
                Status::Stopped => break,
                Status::Unverified => sleep(Duration::from_millis(50)).await,
            }
        }
        ensure!(
            read(&self.cluster.root.join("data/PG_VERSION"))?
                .trim()
                .parse::<u32>()?
                == self.cluster.manifest.major,
            "cluster major changed; explicit upgrade required"
        );
        let version = output(
            command(&self.cluster.manifest.bin.join("postgres")).arg("--version"),
            Duration::from_secs(5),
        )
        .await?;
        ensure!(
            version == self.cluster.manifest.binary_version,
            "managed binary changed; explicit upgrade required"
        );
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.cluster.root.join("logs/postgres.log"))?;
        ensure!(
            log.metadata()?.is_file(),
            "managed log must be regular file"
        );
        let mut cmd = command(&self.cluster.manifest.bin.join("postgres"));
        cmd.arg("-D")
            .arg(self.cluster.root.join("data"))
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(false);
        cmd.process_group(0);
        self.child = Some(cmd.spawn()?);
        self.cluster.wait_ready(duration).await
    }

    /// Smart shutdown drains clients. Timeout leaves the shutdown in progress;
    /// it never escalates to an unverified PID or deletes cluster files.
    pub async fn stop(&mut self, duration: Duration) -> Result<()> {
        match self.cluster.status().await? {
            Status::Stopped => return Ok(()),
            Status::Unverified => bail!("refusing to stop an unverified server"),
            Status::Ready { .. } => {}
        }
        output(
            command(&self.cluster.manifest.bin.join("pg_ctl"))
                .arg("-D")
                .arg(self.cluster.root.join("data"))
                .args([
                    "stop",
                    "-m",
                    "smart",
                    "-w",
                    "-t",
                    &duration.as_secs().max(1).to_string(),
                ]),
            duration + Duration::from_secs(1),
        )
        .await?;
        if let Some(mut child) = self.child.take() {
            child.wait().await?;
        }
        Ok(())
    }
}
