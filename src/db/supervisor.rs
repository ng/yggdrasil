//! Persistent owner and bounded private-socket control protocol. A stop request
//! is never resent after delivery: a lost response has an unknown outcome.
use super::runtime::{ManagedCluster, Status};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Command,
    time::{sleep, timeout},
};
use uuid::Uuid;

const VERSION: u32 = 1;
const FRAME_LIMIT: usize = 4096;
const PROBE: Duration = Duration::from_secs(4);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Action {
    Status,
    Stop { seconds: u64 },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    cluster_id: Uuid,
    action: Action,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub cluster_id: Uuid,
    pub supervisor_pid: u32,
    pub postgres: Status,
    pub error: Option<String>,
}

fn socket(cluster: &ManagedCluster) -> PathBuf {
    cluster.root().join("runtime/control.sock")
}

async fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut bytes = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return Ok(serde_json::from_slice(&bytes)?);
        }
        ensure!(bytes.len() < FRAME_LIMIT, "supervisor frame exceeds limit");
        bytes.push(byte);
    }
}

async fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= FRAME_LIMIT, "supervisor frame exceeds limit");
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn exchange(
    cluster: &ManagedCluster,
    mut stream: UnixStream,
    action: Action,
    duration: Duration,
) -> Result<Reply> {
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "supervisor socket owner mismatch"
    );
    timeout(duration, async {
        write_frame(
            &mut stream,
            &Request {
                version: VERSION,
                cluster_id: cluster.id(),
                action,
            },
        )
        .await?;
        let reply: Reply = read_frame(&mut stream).await?;
        ensure!(
            reply.version == VERSION && reply.cluster_id == cluster.id(),
            "supervisor protocol or cluster mismatch"
        );
        if let Some(error) = &reply.error {
            anyhow::bail!("{error}");
        }
        Ok(reply)
    })
    .await
    .context("supervisor reply timed out; operation outcome unknown")?
}

/// Read-only IPC probe. Failure never starts a supervisor or PostgreSQL.
pub async fn inspect(cluster: &ManagedCluster) -> Result<Reply> {
    timeout(PROBE, async {
        let stream = UnixStream::connect(socket(cluster)).await?;
        exchange(cluster, stream, Action::Status, PROBE).await
    })
    .await
    .context("supervisor probe timed out")?
}

/// Start only an already initialized cluster. Pin the selected root and identity
/// in child arguments so changing config/profiles cannot redirect the child.
pub async fn start(
    cluster: &ManagedCluster,
    executable: &Path,
    duration: Duration,
) -> Result<Reply> {
    timeout(duration, async {
        if let Ok(reply) = inspect(cluster).await {
            if matches!(reply.postgres, Status::Ready { .. })
                && cluster.status().await? == reply.postgres
            {
                return Ok(reply);
            }
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(cluster.root().join("logs/supervisor.log"))?;
        let metadata = log.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "supervisor log must be a private owned file"
        );
        let mut child: Option<tokio::process::Child> = None;
        loop {
            if let Ok(reply) = inspect(cluster).await {
                if matches!(reply.postgres, Status::Ready { .. })
                    && cluster.status().await? == reply.postgres
                {
                    return Ok(reply);
                }
            }
            if let Some(process) = &mut child {
                if let Some(status) = process.try_wait()? {
                    ensure!(
                        status.success(),
                        "supervisor exited unsuccessfully; inspect supervisor log"
                    );
                    child = None;
                }
            }
            // A successful losing child may have raced a stopping owner. Only
            // retry after that child is terminal AND the OS lease is available.
            if child.is_none() && cluster.try_owner()?.is_some() {
                child = Some(
                    Command::new(executable)
                        .args(["db", "serve", "--cluster-root"])
                        .arg(cluster.root())
                        .arg("--cluster-id")
                        .arg(cluster.id().to_string())
                        .stdin(Stdio::null())
                        .stdout(log.try_clone()?)
                        .stderr(log.try_clone()?)
                        .kill_on_drop(false)
                        .process_group(0)
                        .spawn()?,
                );
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("managed start timed out; inspect status and supervisor log before retrying")?
}

/// If an owner exists, stop through it. If no owner exists, take the same lease
/// and stop the verified orphan directly, without starting it first.
pub async fn stop(cluster: &ManagedCluster, duration: Duration) -> Result<()> {
    ensure!(
        (1..=300).contains(&duration.as_secs()),
        "stop timeout must be 1–300 seconds"
    );
    timeout(duration + Duration::from_secs(5), async {
        loop {
            match UnixStream::connect(socket(cluster)).await {
                Ok(stream) => {
                    // Do not fall back or resend if delivery/response fails.
                    let reply = exchange(
                        cluster,
                        stream,
                        Action::Stop {
                            seconds: duration.as_secs(),
                        },
                        duration + PROBE,
                    )
                    .await
                    .context("stop may have been delivered; inspect status before retrying")?;
                    ensure!(
                        reply.postgres == Status::Stopped,
                        "supervisor did not confirm shutdown"
                    );
                    return Ok(());
                }
                Err(_) => {
                    if let Some(mut owner) = cluster.try_owner()? {
                        owner.stop(duration).await?;
                        return Ok(());
                    }
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("managed stop timed out; shutdown may still be in progress")?
}

struct Endpoint {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.dev() == self.device && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn bind(cluster: &ManagedCluster) -> Result<(UnixListener, Endpoint)> {
    let path = socket(cluster);
    match fs::symlink_metadata(&path) {
        Ok(m) => {
            ensure!(
                m.file_type().is_socket() && m.uid() == unsafe { libc::geteuid() },
                "refusing to replace unexpected control socket contents"
            );
            fs::remove_file(&path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let m = fs::symlink_metadata(&path)?;
    Ok((
        listener,
        Endpoint {
            path,
            device: m.dev(),
            inode: m.ino(),
        },
    ))
}

fn reply(cluster: &ManagedCluster, postgres: Status, error: Option<String>) -> Reply {
    Reply {
        version: VERSION,
        cluster_id: cluster.id(),
        supervisor_pid: std::process::id(),
        postgres,
        error,
    }
}

/// Foreground lifetime owner. SIGINT/SIGTERM release ownership while leaving
/// PostgreSQL available for adoption. Only the explicit stop protocol drains it.
pub async fn serve(cluster: ManagedCluster) -> Result<()> {
    let Some(mut owner) = cluster.try_owner()? else {
        return Ok(());
    };
    // Bind while owning the lease, before any server start; never replace a
    // live cooperating owner's socket. Endpoint drops before the owner lease.
    let (listener, _endpoint) = bind(&cluster)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    owner.start_or_adopt(Duration::from_secs(30)).await?;
    let mut monitor = tokio::time::interval(Duration::from_secs(1));
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = terminate.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            _ = monitor.tick() => {
                if let Err(error) = owner.start_or_adopt(Duration::from_secs(5)).await {
                    tracing::warn!(%error, "managed server not ready; preserving cluster state");
                }
            }
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                if !stream.peer_cred().is_ok_and(|peer| peer.uid() == unsafe { libc::geteuid() }) { continue; }
                let request: Request = match timeout(Duration::from_secs(2), read_frame(&mut stream)).await {
                    Ok(Ok(request)) => request,
                    _ => continue,
                };
                if request.version != VERSION || request.cluster_id != cluster.id() { continue; }
                match request.action {
                    Action::Status => {
                        let state = cluster.status().await.unwrap_or(Status::Unverified);
                        let _ = timeout(PROBE, write_frame(&mut stream, &reply(&cluster, state, None))).await;
                    }
                    Action::Stop { seconds } => {
                        if !(1..=300).contains(&seconds) { continue; }
                        let result = owner.stop(Duration::from_secs(seconds)).await;
                        let response = match result {
                            Ok(()) => reply(&cluster, Status::Stopped, None),
                            Err(error) => reply(&cluster, Status::Unverified, Some(format!("shutdown incomplete: {error}"))),
                        };
                        let _ = timeout(PROBE, write_frame(&mut stream, &response)).await;
                        // Even on timeout, stop monitoring: the server may still
                        // be draining. Never restart after an explicit stop.
                        return Ok(());
                    }
                }
            }
        }
    }
}
