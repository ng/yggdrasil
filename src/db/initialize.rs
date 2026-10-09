//! Explicit installation, serialized separately from the persistent supervisor.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};

use super::{package, provision, runtime::ManagedCluster, supervisor};

pub async fn run(data_dir: &Path, archive: Option<&Path>, migrations: bool) -> Result<()> {
    ensure!(data_dir.is_absolute(), "absolute data directory required");
    // Reject unsupported targets and invalid offline bytes before creating state.
    let target = package::current_target()?;
    if let Some(archive) = archive {
        package::verify_archive(archive, target)?;
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)?;
    let metadata = fs::symlink_metadata(data_dir)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "managed data directory must be private and owned"
    );
    let data_dir = data_dir.canonicalize()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(data_dir.join(".initialize.lock"))?;
    let metadata = lock.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "initialization lock must be a private owned file"
    );
    tokio::time::timeout(Duration::from_secs(240), async {
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => return Ok::<_, std::io::Error>(()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("another initialization is still running; retry later")??;
    let root = data_dir.join("postgres");
    let cluster =
        if root.join("cluster.json").try_exists()? || root.join(".bootstrap.json").try_exists()? {
            ManagedCluster::resume_initialization(&root)
                .await
                .context("managed bootstrap recovery failed; existing files retained")?
        } else {
            let base = data_dir.join("binaries");
            let bin = match archive {
                Some(archive) => {
                    let archive = archive.to_owned();
                    tokio::task::spawn_blocking(move || package::install_offline(&base, &archive))
                        .await??
                }
                None => package::install_download(&base).await?,
            };
            ManagedCluster::initialize(&root, &bin, 16).await?
        };
    supervisor::start(&cluster, &std::env::current_exe()?, Duration::from_secs(30)).await?;
    if migrations {
        provision::migrate(&cluster).await?;
    }
    Ok(())
}
