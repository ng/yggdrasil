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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub(super) version: u32,
    pub(super) root: PathBuf,
    pub(super) cluster_id: Uuid,
    pub(super) system_id: String,
    pub(super) major: u32,
    pub(super) binary_version: String,
    pub(super) bin: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) socket_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ManagedCluster {
    root: PathBuf,
    manifest: Manifest,
    maintenance: Option<super::upgrade::Permit>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
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
    #[cfg(test)]
    if executable.file_name().is_some_and(|n| n == "initdb")
        && std::env::var("YGG_BOOTSTRAP_TEST_PHASE").as_deref() == Ok("orphan")
    {
        // The orphan must be able to finish after its parent's pipes disappear.
        let root = std::env::var("YGG_BOOTSTRAP_TEST_ROOT").unwrap();
        let log = File::create(Path::new(&root).join("orphan-initdb.log")).unwrap();
        cmd.stdout(log.try_clone().unwrap()).stderr(log);
    }
    cmd
}

async fn output(cmd: &mut Command, duration: Duration) -> Result<String> {
    #[cfg(test)]
    if Path::new(cmd.as_std().get_program())
        .file_name()
        .is_some_and(|n| n == "initdb")
        && std::env::var("YGG_BOOTSTRAP_TEST_PHASE").as_deref() == Ok("orphan")
    {
        // Command::output replaces explicit stdio with pipes. Preserve the
        // orphan fixture's files, which must survive the bootstrap parent's death.
        let mut child = cmd.spawn()?;
        ensure!(
            timeout(duration, child.wait()).await??.success(),
            "fixture initdb failed"
        );
        return Ok(String::new());
    }
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapReady {
    manifest: Manifest,
    attempt: Uuid,
}

fn publish_metadata(root: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let temp = root.join(format!(".bootstrap-tmp-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    super::package::publish(&temp, &root.join(name))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn bootstrap_checkpoint(_root: &Path, _phase: &str) {
    #[cfg(test)]
    if std::env::var("YGG_BOOTSTRAP_TEST_PHASE").as_deref() == Ok(_phase) {
        fs::write(_root.join("test-checkpoint"), _phase).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl ManagedCluster {
    /// Explicit bootstrap only. Never invoked by status, hooks or open. Interrupted
    /// attempts are retained; only a successful durable receipt can publish data.
    pub async fn initialize(root: &Path, bin: &Path, major: u32) -> Result<Self> {
        ensure!(
            root.is_absolute() && bin.is_absolute(),
            "absolute managed paths required"
        );
        private_dir(root, true)?;
        let root = root.canonicalize()?;
        super::upgrade::check(&root, None, None)?;
        let _lease = lease(&root)?.context("managed cluster is owned by another process")?;
        ensure!(
            !root.join("cluster.json").try_exists()?,
            "managed cluster already initialized"
        );
        let intent_path = root.join(".bootstrap.json");
        if !intent_path.try_exists()? {
            ensure!(
                fs::read_dir(&root)?.all(|entry| entry.is_ok_and(|e| {
                    let name = e.file_name();
                    name == ".owner.lock"
                        || name
                            .to_str()
                            .and_then(|n| n.strip_prefix(".bootstrap-tmp-"))
                            .is_some_and(|id| Uuid::parse_str(id).is_ok())
                })),
                "managed root is not empty; refusing initialization"
            );
        }
        let bin = bin.canonicalize()?;
        let binary_version = output(
            command(&bin.join("postgres")).arg("--version"),
            // Fresh native packages can take several seconds to load before
            // even --version responds. Keep bootstrap bounded without treating
            // the normal readiness probe budget as a cold executable deadline.
            Duration::from_secs(30),
        )
        .await
        .context("cannot inspect selected PostgreSQL version during bootstrap")?;
        ensure!(
            binary_version
                .split_whitespace()
                .nth(2)
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
                == Some(major),
            "selected PostgreSQL binary major differs from requested major"
        );
        let intent = if intent_path.try_exists()? {
            let intent: Manifest = serde_json::from_str(&read(&intent_path)?)?;
            ensure!(
                intent.version == 1
                    && intent.root == root
                    && intent.bin == bin
                    && intent.major == major
                    && intent.binary_version == binary_version
                    && intent.system_id.is_empty(),
                "bootstrap identity or selected binary changed"
            );
            intent
        } else {
            let cluster_id = Uuid::new_v4();
            let intent = Manifest {
                socket_dir: super::runtime_endpoint::choose(&root, cluster_id)?,
                version: 1,
                root: root.clone(),
                cluster_id,
                system_id: String::new(),
                major,
                binary_version: binary_version.clone(),
                bin: bin.clone(),
            };
            publish_metadata(&root, ".bootstrap.json", &intent)?;
            intent
        };
        super::runtime_endpoint::validate_selection(
            &root,
            intent.cluster_id,
            intent.socket_dir.as_deref(),
        )?;
        bootstrap_checkpoint(&root, "intent");
        if root.join(".bootstrap-ready.json").try_exists()? {
            return Self::finish_bootstrap(&root, &intent).await;
        }
        ensure!(
            !root.join("data").try_exists()?,
            "data exists without completed bootstrap receipt; refusing replacement"
        );
        private_dir(&root.join("runtime"), true)?;
        super::runtime_endpoint::check(
            &root,
            intent.cluster_id,
            intent.socket_dir.as_deref(),
            true,
        )?;
        let socket = super::runtime_endpoint::path(&root, intent.socket_dir.as_deref());
        private_dir(&root.join("logs"), true)?;
        // Never reuse an unfinished attempt: an initdb child may have survived
        // the caller. Its writes stay confined to its unique retained directory.
        let attempt = Uuid::new_v4();
        let data = root.join(format!(".bootstrap-data-{attempt}"));
        private_dir(&data, true)?;
        File::open(&root)?.sync_all()?;
        bootstrap_checkpoint(&root, "attempt");
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
        bootstrap_checkpoint(&root, "initdb");
        private_dir(&data, false)?;
        let control = output(
            command(&bin.join("pg_controldata")).arg(&data),
            Duration::from_secs(30),
        )
        .await
        .context("cannot inspect initialized PostgreSQL control data")?;
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
            socket_dir: intent.socket_dir.clone(),
            version: 1,
            root: root.clone(),
            cluster_id: intent.cluster_id,
            system_id,
            major,
            binary_version,
            bin,
        };
        publish_metadata(
            &root,
            ".bootstrap-ready.json",
            &BootstrapReady { manifest, attempt },
        )?;
        bootstrap_checkpoint(&root, "ready");
        Self::finish_bootstrap(&root, &intent).await
    }

    async fn finish_bootstrap(root: &Path, intent: &Manifest) -> Result<Self> {
        let ready: BootstrapReady =
            serde_json::from_str(&read(&root.join(".bootstrap-ready.json"))?)?;
        let manifest = ready.manifest;
        ensure!(
            manifest.version == intent.version
                && manifest.root == intent.root
                && manifest.cluster_id == intent.cluster_id
                && manifest.bin == intent.bin
                && manifest.major == intent.major
                && manifest.binary_version == intent.binary_version
                && manifest.socket_dir == intent.socket_dir,
            "bootstrap receipt differs from intent"
        );
        let stage = root.join(format!(".bootstrap-data-{}", ready.attempt));
        let data = root.join("data");
        ensure!(
            !(stage.try_exists()? && data.try_exists()?),
            "both bootstrap stage and published data exist; refusing replacement"
        );
        let source = if data.try_exists()? { &data } else { &stage };
        private_dir(source, false)?;
        ensure!(
            !source.join("postmaster.pid").try_exists()?,
            "bootstrap data has a server PID file; refusing recovery"
        );
        ensure!(
            read(&source.join("PG_VERSION"))?.trim().parse::<u32>()? == manifest.major,
            "bootstrap data major changed"
        );
        let control = output(
            command(&manifest.bin.join("pg_controldata")).arg(source),
            Duration::from_secs(30),
        )
        .await
        .context("cannot verify retained PostgreSQL bootstrap control data")?;
        ensure!(
            control
                .lines()
                .find_map(|line| line.strip_prefix("Database system identifier:"))
                .map(str::trim)
                == Some(manifest.system_id.as_str())
                && manifest.system_id.parse::<u64>().is_ok(),
            "bootstrap data identity changed"
        );
        if source == &stage {
            super::package::publish(&stage, &data)?;
            File::open(root)?.sync_all()?;
        }
        bootstrap_checkpoint(root, "data");
        publish_metadata(root, "cluster.json", &manifest)?;
        bootstrap_checkpoint(root, "manifest");
        Self::open(root)
    }

    /// Resume only a durable bootstrap intent, never infer authority from an
    /// arbitrary existing data directory. Complete clusters remain unchanged.
    pub async fn resume_initialization(root: &Path) -> Result<Self> {
        if root.join("cluster.json").try_exists()? {
            return Self::open(root);
        }
        private_dir(root, false)?;
        let intent: Manifest = serde_json::from_str(&read(&root.join(".bootstrap.json"))?)?;
        ensure!(
            intent.version == 1
                && intent.root == root.canonicalize()?
                && intent.bin.is_absolute()
                && intent.system_id.is_empty(),
            "invalid bootstrap intent"
        );
        Self::initialize(root, &intent.bin, intent.major).await
    }

    /// Inspect existing metadata only. Does not create directories or start a server.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_inner(root, None)
    }

    pub(super) fn open_for_upgrade(root: &Path, permit: super::upgrade::Permit) -> Result<Self> {
        Self::open_inner(root, Some(permit))
    }

    fn open_inner(root: &Path, maintenance: Option<super::upgrade::Permit>) -> Result<Self> {
        private_dir(root, false)?;
        let root = root.canonicalize()?;
        let manifest: Manifest = serde_json::from_str(&read(&root.join("cluster.json"))?)?;
        super::upgrade::check(&root, Some(&manifest), maintenance.as_ref())?;
        ensure!(
            manifest.version == 1
                && manifest.root == root
                && manifest.bin.is_absolute()
                && manifest.system_id.parse::<u64>().is_ok(),
            "invalid cluster manifest or relocated cluster; explicit move required"
        );
        super::runtime_endpoint::check(
            &root,
            manifest.cluster_id,
            manifest.socket_dir.as_deref(),
            false,
        )?;
        for dir in ["data", "runtime", "logs"] {
            private_dir(&root.join(dir), false)?;
        }
        ensure!(
            read(&root.join("data/PG_VERSION"))?.trim().parse::<u32>()? == manifest.major,
            "cluster major differs from manifest; explicit upgrade required"
        );
        Ok(Self {
            root,
            manifest,
            maintenance,
        })
    }

    pub(super) fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub(super) fn verify_selection(&self) -> Result<()> {
        super::upgrade::check(&self.root, Some(&self.manifest), self.maintenance.as_ref())?;
        let current: Manifest = serde_json::from_str(&read(&self.root.join("cluster.json"))?)?;
        ensure!(
            current == self.manifest,
            "managed cluster metadata changed; reopen the selected cluster before retrying"
        );
        Ok(())
    }

    pub(super) async fn patch_manifest(&self, bin: &Path) -> Result<Manifest> {
        let bin = bin.canonicalize()?;
        let binary_version = output(
            command(&bin.join("postgres")).arg("--version"),
            Duration::from_secs(30),
        )
        .await?;
        let mut target = self.manifest.clone();
        target.bin = bin;
        target.binary_version = binary_version;
        Ok(target)
    }

    pub(super) fn bin(&self) -> &Path {
        &self.manifest.bin
    }

    /// Selected PostgreSQL and supervisor endpoint directory. Inspection only;
    /// this path may be absent after temporary-directory cleanup while stopped.
    pub fn socket_dir(&self) -> PathBuf {
        super::runtime_endpoint::path(&self.root, self.manifest.socket_dir.as_deref())
    }

    pub fn id(&self) -> Uuid {
        self.manifest.cluster_id
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// Administrative connection for lifecycle verification only. Application
    /// connections must use the separately provisioned limited runtime role.
    pub(super) fn admin_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(self.socket_dir().to_str().unwrap())
            .port(5432)
            .username(BOOTSTRAP)
            .database("postgres")
            .application_name("ygg-managed-supervisor")
    }

    async fn verified_pid(&self) -> Result<i32> {
        super::runtime_endpoint::check(
            &self.root,
            self.id(),
            self.manifest.socket_dir.as_deref(),
            false,
        )?;
        let before = read(&self.root.join("data/postmaster.pid"))?;
        let lines: Vec<_> = before.lines().collect();
        ensure!(lines.len() >= 8, "incomplete server PID file");
        let pid: i32 = lines[0].parse()?;
        let start: i64 = lines[2].parse()?;
        ensure!(
            pid > 1
                && start > 0
                && Path::new(lines[1]) == self.root.join("data")
                && lines[3] == "5432"
                && Path::new(lines[4]) == self.socket_dir(),
            "server PID identity mismatch"
        );
        let mut conn = PgConnection::connect_with(&self.admin_options()).await?;
        let (directory, system_id, version, listen, backend, sockets, permissions): (String, String, i32, String, i32, String, String) = sqlx::query_as(
            "SELECT current_setting('data_directory'), system_identifier::text, current_setting('server_version_num')::int, current_setting('listen_addresses'), pg_backend_pid(), current_setting('unix_socket_directories'), current_setting('unix_socket_permissions') FROM pg_control_system()")
            .fetch_one(&mut conn).await?;
        ensure!(
            Path::new(&directory) == self.root.join("data")
                && system_id == self.manifest.system_id
                && version / 10000 == self.manifest.major as i32
                && listen.is_empty()
                && Path::new(&sockets) == self.socket_dir()
                && u32::from_str_radix(&permissions, 8).ok() == Some(0o700),
            "live server identity mismatch"
        );
        // PostgreSQL writes MyStartTime into the PID file, but the SQL function
        // pg_postmaster_start_time() returns PgStartTime captured later (after
        // shared-preload initialization). Equality rejects healthy slow starts.
        // See PostgreSQL src/backend/postmaster/postmaster.c and miscinit.c.
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
        let Some(lease) = lease(&self.root)? else {
            return Ok(None);
        };
        // A caller can wait while maintenance changes the selected binaries or
        // cluster identity. Only the metadata observed under ownership may
        // authorize lifecycle operations; never revive a cached selection.
        self.verify_selection()?;
        Ok(Some(Owner {
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
    pub(super) async fn select_patch(&mut self, target: &Manifest) -> Result<()> {
        self.cluster.verify_selection()?;
        ensure!(
            self.cluster.maintenance.is_some(),
            "patch selection requires maintenance authority"
        );
        let mut compatible = self.cluster.manifest.clone();
        compatible.bin = target.bin.clone();
        compatible.binary_version = target.binary_version.clone();
        ensure!(
            compatible == *target,
            "patch upgrade cannot change cluster identity or major"
        );
        ensure!(
            self.cluster.status().await? == Status::Stopped,
            "patch selection requires a verified stopped server"
        );
        super::upgrade::replace(&self.cluster.root, "cluster.json", target)?;
        self.cluster.manifest = target.clone();
        Ok(())
    }

    pub(super) async fn prepare_endpoint(&self) -> Result<()> {
        let missing_directory = std::fs::symlink_metadata(self.cluster.socket_dir())
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        let missing_identity = self.cluster.manifest.socket_dir.is_some()
            && std::fs::symlink_metadata(self.cluster.socket_dir().join("cluster.json"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        if missing_directory || missing_identity {
            ensure!(
                self.cluster.status().await? == Status::Stopped,
                "missing endpoint for an unverified live server; refusing recreation"
            );
        }
        super::runtime_endpoint::check(
            &self.cluster.root,
            self.cluster.id(),
            self.cluster.manifest.socket_dir.as_deref(),
            true,
        )
    }

    pub async fn start_or_adopt(&mut self, duration: Duration) -> Result<i32> {
        timeout(duration, self.start_or_adopt_inner(duration))
            .await
            .context("managed startup timed out; server state retained")?
    }

    async fn start_or_adopt_inner(&mut self, duration: Duration) -> Result<i32> {
        loop {
            self.cluster.verify_selection()?;
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
        self.prepare_endpoint().await?;
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
        let control = output(
            command(&self.cluster.manifest.bin.join("pg_controldata"))
                .arg(self.cluster.root.join("data")),
            Duration::from_secs(5),
        )
        .await?;
        ensure!(
            control
                .lines()
                .find_map(|line| line.strip_prefix("Database system identifier:"))
                .map(str::trim)
                == Some(self.cluster.manifest.system_id.as_str()),
            "cluster data identity changed; refusing to start a different database"
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
        self.cluster.verify_selection()?;
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

#[cfg(test)]
mod ownership_metadata_tests {
    use super::*;

    fn fixture(root: &Path) -> ManagedCluster {
        let root = root.canonicalize().unwrap();
        let cluster = ManagedCluster {
            maintenance: None,
            manifest: Manifest {
                version: 1,
                root: root.clone(),
                cluster_id: Uuid::new_v4(),
                system_id: "1234".into(),
                major: 16,
                binary_version: "postgres (PostgreSQL) 16.14".into(),
                bin: root.join("old-bin"),
                socket_dir: None,
            },
            root,
        };
        publish_metadata(&cluster.root, "cluster.json", &cluster.manifest).unwrap();
        cluster
    }

    #[test]
    fn waiting_handle_cannot_own_a_changed_binary_selection() {
        let temp = tempfile::tempdir().unwrap();
        let old = fixture(temp.path());
        let maintenance = old.try_owner().unwrap().unwrap();
        let mut selected = old.clone();
        selected.manifest.bin = old.root.join("new-bin");
        selected.manifest.binary_version = "postgres (PostgreSQL) 16.15".into();
        fs::write(
            old.root.join("cluster.json"),
            serde_json::to_vec(&selected.manifest).unwrap(),
        )
        .unwrap();
        assert!(old.try_owner().unwrap().is_none());
        drop(maintenance);
        assert!(old.try_owner().is_err());
        // Refusal releases ownership, so a newly resolved selection can proceed.
        assert!(selected.try_owner().unwrap().is_some());
    }

    #[test]
    fn unreadable_or_changed_identity_never_grants_cached_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let cluster = fixture(temp.path());
        let path = cluster.root.join("cluster.json");
        fs::remove_file(&path).unwrap();
        assert!(cluster.try_owner().is_err());
        fs::write(&path, "{unfinished").unwrap();
        assert!(cluster.try_owner().is_err());
        let mut replaced = cluster.manifest.clone();
        replaced.cluster_id = Uuid::new_v4();
        fs::write(&path, serde_json::to_vec(&replaced).unwrap()).unwrap();
        assert!(cluster.try_owner().is_err());
        fs::write(&path, serde_json::to_vec_pretty(&cluster.manifest).unwrap()).unwrap();
        assert!(cluster.try_owner().unwrap().is_some());
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn bootstrap_worker() {
        let Ok(root) = std::env::var("YGG_BOOTSTRAP_TEST_ROOT") else {
            return;
        };
        let bin = std::env::var("YGG_TEST_PG_BIN").unwrap();
        let major = std::env::var("YGG_TEST_PG_MAJOR").unwrap().parse().unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ManagedCluster::initialize(
                Path::new(&root),
                Path::new(&bin),
                major,
            ))
            .unwrap();
    }

    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "requires YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR"]
    fn bootstrap_live_initdb_orphan() {
        let temp = tempfile::Builder::new()
            .prefix("ybo-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap().join("cluster");
        let mut worker = Worker(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "db::runtime::bootstrap_tests::bootstrap_worker",
                    "--nocapture",
                ])
                .env("YGG_BOOTSTRAP_TEST_ROOT", &root)
                .env("YGG_BOOTSTRAP_TEST_PHASE", "orphan")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let (attempt, pid) = loop {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "bootstrap finished before orphan capture"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "initdb never reached data creation"
            );
            let attempt = fs::read_dir(&root).ok().and_then(|entries| {
                entries.filter_map(Result::ok).map(|e| e.path()).find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".bootstrap-data-")
                        && p.join("PG_VERSION").exists()
                })
            });
            if let Some(attempt) = attempt {
                // Both supported Unix families provide these POSIX ps columns.
                let ps = std::process::Command::new("/bin/ps")
                    .args(["-axo", "pid=,ppid=,comm="])
                    .output()
                    .unwrap();
                assert!(ps.status.success());
                let pid = String::from_utf8(ps.stdout)
                    .unwrap()
                    .lines()
                    .find_map(|line| {
                        let mut fields = line.split_whitespace();
                        let pid = fields.next()?.parse::<i32>().ok()?;
                        let parent = fields.next()?.parse::<u32>().ok()?;
                        let executable = fields.next()?;
                        (parent == worker.0.id() && Path::new(executable).file_name()? == "initdb")
                            .then_some(pid)
                    });
                if let Some(pid) = pid {
                    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
                    break (attempt, pid);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        // On failure, only signal the captured child while it is still stopped.
        // After continuation it may exit, so never signal a potentially reused PID.
        struct StoppedOrphan(Option<i32>);
        impl Drop for StoppedOrphan {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        let mut orphan = StoppedOrphan(Some(pid));
        // Confirm SIGSTOP was delivered before killing the parent.
        loop {
            let ps = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&ps.stdout).contains('T') {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "initdb did not stop");
            std::thread::sleep(Duration::from_millis(1));
        }
        worker.0.kill().unwrap();
        worker.0.wait().unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "real initdb did not survive parent"
        );
        let intent: Manifest =
            serde_json::from_str(&read(&root.join(".bootstrap.json")).unwrap()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let cluster = runtime
            .block_on(ManagedCluster::resume_initialization(&root))
            .unwrap();
        assert_eq!(cluster.id(), intent.cluster_id);
        assert!(attempt.exists());
        let published = fs::read(root.join("data/global/pg_control")).unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        orphan.0 = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let log = fs::read_to_string(root.join("orphan-initdb.log")).unwrap();
            if log.contains("Success. You can now start the database server") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "orphan initdb did not finish: {log}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            attempt.join("global/pg_control").exists(),
            "orphan must finish its abandoned cluster"
        );
        assert_eq!(
            fs::read(root.join("data/global/pg_control")).unwrap(),
            published,
            "late orphan writes changed published cluster"
        );
        assert_ne!(
            fs::read(attempt.join("global/pg_control")).unwrap(),
            published
        );
        assert_eq!(ManagedCluster::open(&root).unwrap().id(), intent.cluster_id);
        assert_eq!(runtime.block_on(cluster.status()).unwrap(), Status::Stopped);
        if cluster.manifest.socket_dir.is_some() {
            fs::remove_dir_all(cluster.socket_dir()).unwrap();
        }
    }

    #[test]
    #[ignore = "requires YGG_TEST_PG_BIN and YGG_TEST_PG_MAJOR"]
    fn bootstrap_crash_recovery() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for phase in ["intent", "attempt", "initdb", "ready", "data", "manifest"] {
            let temp = tempfile::Builder::new()
                .prefix("ybr-")
                .tempdir_in("/tmp")
                .unwrap();
            let name = if matches!(phase, "attempt" | "ready" | "manifest") {
                "long-cluster-".to_owned() + &"x".repeat(120)
            } else {
                "cluster".to_owned()
            };
            let root = temp.path().canonicalize().unwrap().join(name);
            let mut child = Worker(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "db::runtime::bootstrap_tests::bootstrap_worker",
                        "--nocapture",
                    ])
                    .env("YGG_BOOTSTRAP_TEST_ROOT", &root)
                    .env("YGG_BOOTSTRAP_TEST_PHASE", phase)
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !root.join("test-checkpoint").exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "worker exited before {phase}"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker stalled before {phase}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            let intent: Manifest =
                serde_json::from_str(&read(&root.join(".bootstrap.json")).unwrap()).unwrap();
            let ready = fs::read(root.join(".bootstrap-ready.json")).ok();
            let retained: Vec<_> = fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".bootstrap-data-")
                })
                .collect();
            let cluster = runtime
                .block_on(ManagedCluster::resume_initialization(&root))
                .unwrap();
            assert_eq!(cluster.id(), intent.cluster_id, "{phase}");
            assert_eq!(cluster.manifest.socket_dir, intent.socket_dir, "{phase}");
            assert_eq!(runtime.block_on(cluster.status()).unwrap(), Status::Stopped);
            if let Some(ready) = ready {
                let before: BootstrapReady = serde_json::from_slice(&ready).unwrap();
                assert_eq!(before.manifest.system_id, cluster.manifest.system_id);
                assert_eq!(fs::read(root.join(".bootstrap-ready.json")).unwrap(), ready);
            } else {
                for attempt in retained {
                    assert!(attempt.is_dir(), "unfinished attempt must be retained");
                    // A surviving initdb cannot write into the newly selected data.
                    fs::write(attempt.join("late-child-write"), "retained").unwrap();
                    assert!(!root.join("data/late-child-write").exists());
                }
            }
            if cluster.manifest.socket_dir.is_some() {
                fs::remove_dir_all(cluster.socket_dir()).unwrap();
                assert_eq!(ManagedCluster::open(&root).unwrap().id(), cluster.id());
            }
            // A completed receipt cannot authorize replacement of changed data.
            if phase == "data" {
                fs::rename(root.join("cluster.json"), root.join("cluster.saved")).unwrap();
                fs::write(root.join("data/PG_VERSION"), "999\n").unwrap();
                assert!(
                    runtime
                        .block_on(ManagedCluster::resume_initialization(&root))
                        .is_err()
                );
                assert_eq!(
                    fs::read_to_string(root.join("data/PG_VERSION")).unwrap(),
                    "999\n"
                );
            }
        }
    }
}
