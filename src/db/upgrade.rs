//! Explicit managed patch upgrades. The journal is also a persistent startup
//! fence; only an operation-bound capability permits maintenance lifecycle work.
use super::{
    deployment_backup, package, restore,
    runtime::{ManagedCluster, Manifest, Owner, Status},
    supervisor,
};
use crate::config::database::{DatabaseTarget, DeploymentConfig};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sqlx::Connection;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const JOURNAL: &str = "upgrade.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    BackedUp,
    Stopped,
    Switched,
    Complete,
    Aborted,
}

impl Phase {
    fn name(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::BackedUp => "backed_up",
            Self::Stopped => "stopped",
            Self::Switched => "switched",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    operation: Uuid,
    source: Manifest,
    target: Manifest,
    backup: PathBuf,
    configuration_sha256: String,
    backup_sha256: Option<String>,
    phase: Phase,
}

#[derive(Clone, Debug)]
pub(super) struct Permit {
    operation: Uuid,
    source: Manifest,
    target: Manifest,
    backup: PathBuf,
    configuration_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub operation: Uuid,
    pub cluster_id: Uuid,
    pub source_version: String,
    pub target_version: String,
    pub backup: PathBuf,
    pub complete: bool,
    pub aborted: bool,
    pub phase: String,
}

pub fn inspect(root: &Path) -> Result<Option<Receipt>> {
    let root = root.canonicalize()?;
    let meta = fs::metadata(&root)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "upgrade root must be private and owned"
    );
    Ok(Journal::load(&root)?.map(|journal| journal.receipt()))
}

pub struct Request<'a> {
    pub backup: &'a Path,
    pub archive: Option<&'a Path>,
    pub resume: Option<Uuid>,
    pub abort: bool,
    pub quiesced: bool,
    pub duration: Duration,
    pub executable: &'a Path,
}

fn private_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "upgrade metadata must be a private owned regular file"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "upgrade metadata exceeds limit");
    Ok(bytes)
}

pub(super) fn replace(root: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let path = root.join(format!(".upgrade-tmp-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    fs::rename(&path, root.join(name))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn version(text: &str) -> Result<(u32, u32)> {
    let number = text
        .strip_prefix("postgres (PostgreSQL) ")
        .and_then(|s| s.split_whitespace().next())
        .context("unsupported managed PostgreSQL version")?;
    let (major, minor) = number.split_once('.').context("patch version required")?;
    Ok((major.parse()?, minor.parse()?))
}

impl Journal {
    fn load(root: &Path) -> Result<Option<Self>> {
        match fs::symlink_metadata(root.join(JOURNAL)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }
        let value: Self = serde_json::from_slice(&private_bytes(&root.join(JOURNAL))?)?;
        let mut compatible = value.source.clone();
        compatible.bin = value.target.bin.clone();
        compatible.binary_version = value.target.binary_version.clone();
        let old = version(&value.source.binary_version)?;
        let new = version(&value.target.binary_version)?;
        ensure!(
            value.version == 1
                && !value.operation.is_nil()
                && value.source.root == root
                && compatible == value.target
                && old.0 == value.source.major
                && new.0 == old.0
                && new.1 > old.1
                && value.backup.is_absolute()
                && !value.backup.starts_with(root)
                && value.configuration_sha256.len() == 64
                && (matches!(value.phase, Phase::Prepared | Phase::Aborted)
                    || value.backup_sha256.as_ref().is_some_and(|h| h.len() == 64)),
            "invalid upgrade journal; retain state for inspection"
        );
        Ok(Some(value))
    }
    fn permit(&self) -> Permit {
        Permit {
            operation: self.operation,
            source: self.source.clone(),
            target: self.target.clone(),
            backup: self.backup.clone(),
            configuration_sha256: self.configuration_sha256.clone(),
        }
    }
    fn save(&self) -> Result<()> {
        replace(&self.source.root, JOURNAL, self)?;
        #[cfg(test)]
        if std::env::var("YGG_UPGRADE_TEST_ROOT").ok().as_deref() == self.source.root.to_str()
            && std::env::var("YGG_UPGRADE_TEST_PHASE").ok().as_deref() == Some(self.phase.name())
        {
            fs::write(
                self.source.root.join("test-upgrade-checkpoint"),
                self.phase.name(),
            )?;
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Ok(())
    }
    fn receipt(&self) -> Receipt {
        Receipt {
            operation: self.operation,
            cluster_id: self.source.cluster_id,
            source_version: self.source.binary_version.trim().into(),
            target_version: self.target.binary_version.trim().into(),
            backup: self.backup.clone(),
            complete: self.phase == Phase::Complete,
            aborted: self.phase == Phase::Aborted,
            phase: self.phase.name().into(),
        }
    }
}

pub(super) fn check(
    root: &Path,
    manifest: Option<&Manifest>,
    permit: Option<&Permit>,
) -> Result<()> {
    let Some(journal) = Journal::load(root)? else {
        ensure!(permit.is_none(), "upgrade maintenance journal disappeared");
        return Ok(());
    };
    if matches!(journal.phase, Phase::Complete | Phase::Aborted) && permit.is_none() {
        let selected = if journal.phase == Phase::Complete {
            &journal.target
        } else {
            &journal.source
        };
        ensure!(
            manifest == Some(selected),
            "completed upgrade selection changed"
        );
        return Ok(());
    }
    let permit = permit.context(format!(
        "managed upgrade {} is pending; inspect and explicitly resume it",
        journal.operation
    ))?;
    ensure!(
        permit.operation == journal.operation
            && permit.source == journal.source
            && permit.target == journal.target
            && permit.backup == journal.backup
            && permit.configuration_sha256 == journal.configuration_sha256,
        "upgrade maintenance authority changed"
    );
    ensure!(
        manifest.is_some_and(|m| match journal.phase {
            Phase::Prepared | Phase::BackedUp | Phase::Aborted => m == &journal.source,
            Phase::Stopped => m == &journal.source || m == &journal.target,
            Phase::Switched | Phase::Complete => m == &journal.target,
        }),
        "upgrade cluster selection differs from journal"
    );
    Ok(())
}

async fn owner(cluster: &ManagedCluster, duration: Duration) -> Result<Owner> {
    tokio::time::timeout(duration, async {
        loop {
            if let Some(owner) = cluster.try_owner()? {
                return Ok(owner);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("upgrade ownership wait timed out; state retained")?
}

fn options(cluster: &ManagedCluster) -> sqlx::postgres::PgConnectOptions {
    cluster
        .admin_options()
        .username("ygg_owner")
        .database("ygg")
}

async fn verify_server_patch(cluster: &ManagedCluster, expected: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection = sqlx::PgConnection::connect_with(&options(cluster)).await?;
        let number: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
            .fetch_one(&mut connection)
            .await?;
        ensure!(
            (number as u32 / 10000, number as u32 % 10000) == version(expected)?,
            "running PostgreSQL patch differs from upgrade selection; state retained"
        );
        connection.close().await?;
        Ok(())
    })
    .await
    .context("upgrade version verification timed out")?
}

async fn preflight_source(cluster: &ManagedCluster) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connection = sqlx::PgConnection::connect_with(&options(cluster)).await
            .context("managed upgrade requires a provisioned Yggdrasil owner connection")?;
        let number: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int")
            .fetch_one(&mut connection).await?;
        ensure!((number as u32 / 10000, number as u32 % 10000) == version(&cluster.manifest().binary_version)?,
            "running PostgreSQL patch differs from selected binaries");
        let _: uuid::Uuid = sqlx::query_scalar("SELECT database_id FROM public.knowledge_storage WHERE singleton")
            .fetch_one(&mut connection).await?;
        let others: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid()")
            .fetch_one(&mut connection).await?;
        ensure!(others == 0, "source has other application sessions; quiesce them before upgrade");
        let unsupported: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_extension WHERE extname NOT IN ('plpgsql','uuid-ossp')")
            .fetch_one(&mut connection).await?;
        ensure!(unsupported == 0, "source has extensions outside the qualified managed patch path; operator review is required");
        connection.close().await?;
        Ok(())
    }).await.context("upgrade preflight timed out; no maintenance intent published")?
}

/// Upgrade only to this application's pinned minor release. Existing writers
/// must remain quiesced until completion; this does not certify fleet quiescence.
pub async fn patch(config: &DeploymentConfig, request: Request<'_>) -> Result<Receipt> {
    let Request {
        backup,
        archive,
        resume,
        abort,
        quiesced,
        duration,
        executable,
    } = request;
    ensure!(
        !abort || resume.is_some(),
        "abort requires the exact --resume operation"
    );
    ensure!(
        quiesced,
        "patch upgrade requires --quiesced; stop all application writers first"
    );
    ensure!(
        (1..=300).contains(&duration.as_secs()),
        "upgrade timeout must be 1–300 seconds"
    );
    let DatabaseTarget::ManagedLocal { data_dir } = &config.database else {
        anyhow::bail!("external PostgreSQL upgrades must be performed by its operator or provider");
    };
    let root = data_dir.join("postgres").canonicalize()?;
    let meta = fs::symlink_metadata(&root)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "upgrade root must be private and owned"
    );
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(".upgrade.lock"))?;
    let meta = lease.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "invalid upgrade lock"
    );
    lease
        .try_lock_exclusive()
        .context("another upgrade holds the maintenance lease")?;
    ensure!(
        backup.is_absolute(),
        "absolute upgrade backup destination required"
    );
    let backup = backup
        .parent()
        .context("backup parent required")?
        .canonicalize()?
        .join(backup.file_name().context("backup name required")?);
    ensure!(
        !backup.starts_with(&root) && !root.starts_with(&backup),
        "backup and cluster directories must not overlap"
    );
    let configuration = crate::config::snapshot::Snapshot::capture(config, None)?.encode()?;
    let configuration_sha256 = crate::knowledge::document::digest(&configuration);
    let existing = Journal::load(&root)?;
    let mut journal = if let Some(operation) = resume {
        let journal = existing.context("upgrade journal missing; cannot resume")?;
        ensure!(
            journal.operation == operation
                && journal.backup == backup
                && journal.configuration_sha256 == configuration_sha256,
            "resume operation, backup or configuration differs from upgrade journal"
        );
        journal
    } else {
        ensure!(
            existing
                .as_ref()
                .is_none_or(|j| matches!(j.phase, Phase::Complete | Phase::Aborted)),
            "upgrade pending; inspect journal and supply --resume OPERATION"
        );
        let source = ManagedCluster::open(&root)?;
        let pinned = package::release()?.postgres_version;
        let target_version = version(&format!("postgres (PostgreSQL) {pinned}"))?;
        let source_version = version(&source.manifest().binary_version)?;
        ensure!(
            source_version.0 == target_version.0,
            "major upgrades require a separate data directory and validated restore; this command handles patches only"
        );
        ensure!(
            target_version.1 > source_version.1,
            "selected PostgreSQL is already at or newer than the pinned patch"
        );
        ensure!(
            matches!(source.status().await?, Status::Ready { .. }),
            "source must be running before a new upgrade"
        );
        preflight_source(&source).await?;
        ensure!(
            !backup.try_exists()?,
            "new upgrade requires an unused backup destination"
        );
        let bin = match archive {
            Some(path) => package::install_offline(&data_dir.join("binaries"), path)?,
            None => package::install_download(&data_dir.join("binaries")).await?,
        };
        let target = source.patch_manifest(&bin).await?;
        ensure!(
            version(&target.binary_version)? == target_version,
            "installed target version differs from pin"
        );
        if let Some(old) = existing {
            replace(
                &root,
                &format!("upgrade-finished-{}.json", old.operation),
                &old,
            )?;
        }
        let journal = Journal {
            version: 1,
            operation: Uuid::new_v4(),
            source: source.manifest().clone(),
            target,
            backup,
            configuration_sha256,
            backup_sha256: None,
            phase: Phase::Prepared,
        };
        journal.save()?;
        journal
    };
    let result = if abort {
        abort_upgrade(duration, executable, &mut journal).await
    } else {
        execute(config, archive, duration, executable, &mut journal).await
    };
    result.with_context(|| {
        format!(
            "upgrade {}: state and backup retained; inspect before --resume {}",
            journal.operation, journal.operation
        )
    })
}

async fn abort_upgrade(
    duration: Duration,
    executable: &Path,
    journal: &mut Journal,
) -> Result<Receipt> {
    ensure!(
        matches!(
            journal.phase,
            Phase::Prepared | Phase::BackedUp | Phase::Stopped | Phase::Aborted
        ),
        "cannot abort after binary selection changed; retain data and explicitly resume or recover"
    );
    if journal.phase != Phase::Aborted {
        let source = ManagedCluster::open_for_upgrade(&journal.source.root, journal.permit())?;
        ensure!(
            source.manifest() == &journal.source,
            "cannot abort a changed binary selection"
        );
        ensure!(
            source.status().await? != Status::Unverified,
            "shutdown state is unverified; inspect before aborting"
        );
        journal.phase = Phase::Aborted;
        journal.save()?;
    }
    let source = ManagedCluster::open(&journal.source.root)?;
    supervisor::start(&source, executable, duration).await?;
    verify_server_patch(&source, &journal.source.binary_version).await?;
    Ok(journal.receipt())
}

async fn execute(
    config: &DeploymentConfig,
    archive: Option<&Path>,
    duration: Duration,
    executable: &Path,
    journal: &mut Journal,
) -> Result<Receipt> {
    let root = journal.source.root.clone();
    if matches!(journal.phase, Phase::Complete | Phase::Aborted) {
        let cluster = ManagedCluster::open(&root)?;
        supervisor::start(&cluster, executable, duration).await?;
        verify_server_patch(&cluster, &cluster.manifest().binary_version).await?;
        return Ok(journal.receipt()); // Never compare old rows after completion.
    }
    let bin = match archive {
        Some(path) => package::install_offline(&config.data_dir.join("binaries"), path)?,
        None => package::install_download(&config.data_dir.join("binaries")).await?,
    };
    ensure!(
        bin.canonicalize()? == journal.target.bin,
        "pinned package selection changed during upgrade"
    );
    let cluster = ManagedCluster::open_for_upgrade(&root, journal.permit())?;
    let mut held = None;
    if journal.phase == Phase::Prepared {
        if !matches!(cluster.status().await?, Status::Ready { .. }) {
            ensure!(
                cluster.status().await? == Status::Stopped,
                "source server state is unverified; inspect before resuming"
            );
            supervisor::stop(&cluster, duration).await?;
            let mut acquired = owner(&cluster, duration).await?;
            acquired.start_or_adopt(duration).await?;
            held = Some(acquired);
        }
        if !journal.backup.try_exists()? {
            deployment_backup::create_for_upgrade(config, &journal.backup, journal.permit())
                .await?;
        }
        let manifest = deployment_backup::verify(&journal.backup)?;
        ensure!(
            manifest
                .configuration
                .as_ref()
                .is_some_and(|c| c.sha256 == journal.configuration_sha256),
            "backup configuration differs from upgrade source"
        );
        ensure!(
            manifest.database.server_major == journal.source.major as i32,
            "backup source major differs"
        );
        restore::validate(&options(&cluster), &manifest.database).await?;
        journal.backup_sha256 = Some(crate::knowledge::document::digest(&private_bytes(
            &journal.backup.join("backup.json"),
        )?));
        journal.phase = Phase::BackedUp;
        journal.save()?;
    }
    let backup = deployment_backup::verify(&journal.backup)?;
    ensure!(
        journal.backup_sha256.as_deref()
            == Some(&crate::knowledge::document::digest(&private_bytes(
                &journal.backup.join("backup.json")
            )?)),
        "upgrade backup manifest changed"
    );
    if journal.phase == Phase::BackedUp {
        match cluster.status().await? {
            Status::Ready { .. } => {
                restore::validate(&options(&cluster), &backup.database).await?;
                if let Some(held) = &mut held {
                    held.stop(duration).await?;
                } else {
                    supervisor::stop(&cluster, duration).await?;
                }
            }
            Status::Stopped => supervisor::stop(&cluster, duration).await?,
            Status::Unverified => {
                anyhow::bail!("source shutdown state is unverified; inspect before resuming")
            }
        }
        journal.phase = Phase::Stopped;
        journal.save()?;
    }
    let mut held = match held {
        Some(held) => held,
        None => owner(&cluster, duration).await?,
    };
    if journal.phase == Phase::Stopped {
        held.select_patch(&journal.target).await?;
        journal.phase = Phase::Switched;
        journal.save()?;
    }
    held.start_or_adopt(duration).await?;
    verify_server_patch(&cluster, &journal.target.binary_version).await?;
    restore::validate(&options(&cluster), &backup.database).await?;
    journal.phase = Phase::Complete;
    journal.save()?;
    drop(held); // The ordinary supervisor adopts this verified target server.
    let selected = ManagedCluster::open(&root)?;
    supervisor::start(&selected, executable, duration).await?;
    Ok(journal.receipt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use std::os::unix::fs::DirBuilderExt;

    #[tokio::test]
    #[ignore = "subprocess helper for patch_publication_crashes_resume_without_losing_rows"]
    async fn patch_phase_child() {
        // Fault-injection controls belong to the test harness, not the
        // deployment configuration bound into the durable upgrade journal.
        let config = DeploymentConfig::load(
            std::env::vars()
                .filter(|(key, _)| !key.starts_with("YGG_UPGRADE_TEST_"))
                .collect(),
        )
        .unwrap();
        let backup = PathBuf::from(std::env::var("YGG_UPGRADE_TEST_BACKUP").unwrap());
        let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
        let executable = PathBuf::from(std::env::var("YGG_TEST_YGG_BIN").unwrap());
        patch(
            &config,
            Request {
                backup: &backup,
                archive: Some(&archive),
                resume: None,
                abort: false,
                quiesced: true,
                duration: Duration::from_secs(30),
                executable: &executable,
            },
        )
        .await
        .unwrap();
        panic!("upgrade returned without the requested publication checkpoint");
    }

    #[tokio::test]
    #[ignore = "requires native YGG_TEST_PG_OLD_BIN, YGG_TEST_PG_ARCHIVE and built YGG_TEST_YGG_BIN"]
    async fn patch_publication_crashes_resume_without_losing_rows() {
        let old_bin = PathBuf::from(std::env::var("YGG_TEST_PG_OLD_BIN").unwrap());
        let archive = PathBuf::from(std::env::var("YGG_TEST_PG_ARCHIVE").unwrap());
        let executable = PathBuf::from(std::env::var("YGG_TEST_YGG_BIN").unwrap());
        for case in [
            "prepared",
            "backed_up",
            "stopped",
            "switched",
            "complete",
            "prepared_abort",
            "stopped_abort",
        ] {
            let abort = case.ends_with("_abort");
            let phase = case.strip_suffix("_abort").unwrap_or(case);
            let temp = tempfile::Builder::new()
                .prefix("yup-crash-")
                .tempdir_in("/tmp")
                .unwrap();
            let base = temp.path().canonicalize().unwrap();
            let data = base.join("data");
            fs::DirBuilder::new().mode(0o700).create(&data).unwrap();
            let root = data.join("postgres");
            let source = ManagedCluster::initialize(&root, &old_bin, 16)
                .await
                .unwrap();
            let mut bootstrap = Some(source.try_owner().unwrap().unwrap());
            let mut child: Option<tokio::process::Child> = None;
            let result = std::panic::AssertUnwindSafe(async {
                bootstrap.as_mut().unwrap().start_or_adopt(Duration::from_secs(30)).await.unwrap();
                super::super::provision::migrate(&source).await.unwrap();
                drop(bootstrap.take());
                let mut connection = sqlx::PgConnection::connect_with(&options(&source)).await.unwrap();
                sqlx::raw_sql("CREATE TABLE upgrade_probe (id int PRIMARY KEY, body text NOT NULL); INSERT INTO upgrade_probe VALUES (1, 'preserved')")
                    .execute(&mut connection).await.unwrap();
                connection.close().await.unwrap();
                supervisor::start(&source, &executable, Duration::from_secs(30)).await.unwrap();
                let backup = base.join("backup");
                let configure = |command: &mut tokio::process::Command| {
                    command.env_remove("DATABASE_URL").env_remove("YGG_DATABASE_OWNER_URL")
                        .env("YGG_DB_MODE", "managed").env("YGG_DATA_DIR", &data).env("YGG_CONFIG_DIR", base.join("config"))
                        .env("YGG_KNOWLEDGE_DIR", base.join("knowledge")).env("YGG_KNOWLEDGE_POLICY_DIR", base.join("policy"));
                };
                let log_path = base.join("child.log");
                let log = File::create(&log_path).unwrap();
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                configure(&mut command);
                command.args(["--exact", "db::upgrade::tests::patch_phase_child", "--ignored", "--nocapture"])
                    .env("YGG_UPGRADE_TEST_ROOT", &root).env("YGG_UPGRADE_TEST_BACKUP", &backup).env("YGG_UPGRADE_TEST_PHASE", phase)
                    .stdout(log.try_clone().unwrap()).stderr(log);
                child = Some(command.spawn().unwrap());
                tokio::time::timeout(Duration::from_secs(90), async {
                    loop {
                        if root.join("test-upgrade-checkpoint").exists() { break; }
                        assert!(child.as_mut().unwrap().try_wait().unwrap().is_none(), "{case}: {}", fs::read_to_string(&log_path).unwrap());
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.expect("publication checkpoint deadline");
                child.take().unwrap().kill().await.unwrap(); // Deliberate fault at an observed durable phase.
                let journal = Journal::load(&root).unwrap().unwrap();
                assert_eq!(journal.phase.name(), phase);
                let mut status = tokio::process::Command::new(&executable);
                configure(&mut status);
                let output = status.args(["db", "status", "--json"]).output().await.unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                if phase != "complete" {
                    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                    assert_eq!(status["state"], "upgrade_pending");
                    assert_eq!(status["upgrade"]["phase"], phase);
                    assert!(ManagedCluster::open(&root).is_err());
                } else {
                    let mut connection = sqlx::PgConnection::connect_with(&options(&source)).await.unwrap();
                    sqlx::query("UPDATE upgrade_probe SET body='after completion'").execute(&mut connection).await.unwrap();
                    connection.close().await.unwrap();
                }
                let resume = |aborting: bool| {
                    let mut command = tokio::process::Command::new(&executable);
                    configure(&mut command);
                    command.args(["db", "upgrade", "--quiesced", "--backup"]).arg(&backup)
                        .arg("--postgres-archive").arg(&archive).arg("--resume").arg(journal.operation.to_string()).arg("--json");
                    if aborting { command.arg("--abort"); }
                    command
                };
                if phase == "switched" {
                    assert!(!resume(true).output().await.unwrap().status.success(), "abort must not downgrade selected binaries");
                }
                let output = resume(abort).output().await.unwrap();
                assert!(output.status.success(), "{case}: {}", String::from_utf8_lossy(&output.stderr));
                let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(receipt["complete"], !abort);
                assert_eq!(receipt["aborted"], abort);
                let selected = ManagedCluster::open(&root).unwrap();
                assert_eq!(selected.id(), source.id());
                assert_eq!(selected.manifest().system_id, source.manifest().system_id);
                let mut connection = sqlx::PgConnection::connect_with(&options(&selected)).await.unwrap();
                let body: String = sqlx::query_scalar("SELECT body FROM upgrade_probe WHERE id=1").fetch_one(&mut connection).await.unwrap();
                assert_eq!(body, if phase == "complete" { "after completion" } else { "preserved" });
                let current: String = sqlx::query_scalar("SHOW server_version").fetch_one(&mut connection).await.unwrap();
                assert!(current.starts_with(if abort { "16.14" } else { "16.15" }));
                connection.close().await.unwrap();
                if phase != "prepared" { assert!(backup.join("backup.json").exists()); }
                eprintln!("upgrade publication recovery passed: {case}");
            }).catch_unwind().await;
            if let Some(mut child) = child {
                let _ = child.kill().await;
            }
            drop(bootstrap);
            let selected = match Journal::load(&root).unwrap() {
                Some(journal) => ManagedCluster::open_for_upgrade(&root, journal.permit()).unwrap(),
                None => ManagedCluster::open(&root).unwrap(),
            };
            if let Err(error) = supervisor::stop(&selected, Duration::from_secs(30)).await {
                let retained = temp.keep();
                panic!("retained upgrade crash fixture {retained:?}: {error:#}");
            }
            if let Err(error) = result {
                std::panic::resume_unwind(error);
            }
        }
    }

    #[tokio::test]
    async fn persistent_fence_covers_cached_owners_and_binds_maintenance_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("cluster");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        for name in ["data", "runtime", "logs"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(name))
                .unwrap();
        }
        fs::write(root.join("data/PG_VERSION"), "16").unwrap();
        let source = Manifest {
            version: 1,
            root: root.clone(),
            cluster_id: Uuid::new_v4(),
            system_id: "1234".into(),
            major: 16,
            binary_version: "postgres (PostgreSQL) 16.14".into(),
            bin: root.join("old-bin"),
            socket_dir: None,
        };
        let mut target = source.clone();
        target.bin = root.join("new-bin");
        target.binary_version = "postgres (PostgreSQL) 16.15".into();
        replace(&root, "cluster.json", &source).unwrap();
        let cached = ManagedCluster::open(&root).unwrap();
        let mut old_owner = cached.try_owner().unwrap().unwrap();
        let mut journal = Journal {
            version: 1,
            operation: Uuid::new_v4(),
            source,
            target,
            backup: temp.path().canonicalize().unwrap().join("backup"),
            configuration_sha256: "a".repeat(64),
            backup_sha256: None,
            phase: Phase::Prepared,
        };
        journal.save().unwrap();
        assert!(ManagedCluster::open(&root).is_err());
        assert!(cached.try_owner().unwrap().is_none());
        let error = old_owner
            .start_or_adopt(Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("pending"));
        // Explicit stop remains possible for the existing owner.
        old_owner.stop(Duration::from_secs(1)).await.unwrap();
        drop(old_owner);
        assert!(cached.try_owner().is_err());
        let authorized = ManagedCluster::open_for_upgrade(&root, journal.permit()).unwrap();
        assert!(authorized.try_owner().unwrap().is_some());
        journal.operation = Uuid::new_v4();
        journal.save().unwrap();
        assert!(authorized.try_owner().is_err());
        let authorized = ManagedCluster::open_for_upgrade(&root, journal.permit()).unwrap();
        fs::write(root.join(JOURNAL), "{unfinished").unwrap();
        assert!(ManagedCluster::open(&root).is_err());
        assert!(authorized.try_owner().is_err());
        journal.phase = Phase::Complete;
        journal.backup_sha256 = Some("b".repeat(64));
        journal.save().unwrap();
        assert!(
            ManagedCluster::open(&root).is_err(),
            "completion cannot authorize old selection"
        );
        replace(&root, "cluster.json", &journal.target).unwrap();
        assert!(
            ManagedCluster::open(&root)
                .unwrap()
                .try_owner()
                .unwrap()
                .is_some()
        );
        assert!(cached.try_owner().is_err());
    }
}
