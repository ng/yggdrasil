//! Explicit pinned distribution installation. No runtime/status/hook call path
//! invokes this module. Offline and network inputs share the same verifier.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};
use uuid::Uuid;

const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
const MAX_EXPANDED: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const RECEIPT: &str = ".ygg-package.json";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub schema: u32,
    pub postgres_version: String,
    pub distribution_version: String,
    pub source_commit: String,
    pub source_url: String,
    pub release_url: String,
    packages: Vec<Package>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Package {
    target: String,
    url: String,
    sha256: String,
    bytes: u64,
    root: String,
}

pub fn release() -> Result<Release> {
    let release: Release = serde_json::from_str(include_str!("packages.json"))?;
    ensure!(release.schema == 1, "unsupported package manifest");
    Ok(release)
}

pub fn current_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        _ => anyhow::bail!(
            "managed PostgreSQL distribution unavailable for this target; use external mode"
        ),
    }
}

pub fn package(target: &str) -> Result<Package> {
    release()?
        .packages
        .into_iter()
        .find(|p| p.target == target)
        .context("target is absent from pinned package manifest")
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verify_bytes(package: &Package, bytes: &[u8]) -> Result<()> {
    ensure!(
        package.bytes <= MAX_ARCHIVE && bytes.len() as u64 == package.bytes,
        "archive size differs from pinned release"
    );
    ensure!(
        hash(bytes) == package.sha256,
        "archive SHA-256 differs from pinned release"
    );
    Ok(())
}

fn archive_bytes(archive: &Path, package: &Package) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(archive)?;
    ensure!(file.metadata()?.is_file(), "archive must be a regular file");
    let mut bytes = Vec::new();
    file.take(package.bytes.min(MAX_ARCHIVE) + 1)
        .read_to_end(&mut bytes)?;
    verify_bytes(package, &bytes)?;
    Ok(bytes)
}

/// Audit another advertised target without executing its binaries.
pub fn verify_archive(archive: &Path, target: &str) -> Result<()> {
    archive_bytes(archive, &package(target)?)?;
    Ok(())
}

fn private_dir(path: &Path, create: bool) -> Result<()> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "package directory must be private and owned"
    );
    Ok(())
}

fn normal(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && path.to_str().is_some_and(|s| !s.contains('\\'))
}

fn parents(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        private_dir(&path, true)?;
    }
    Ok(())
}

fn extract(bytes: &[u8], package: &Package, stage: &Path) -> Result<()> {
    let decoded = Read::take(
        flate2::read::GzDecoder::new(Cursor::new(bytes)),
        MAX_EXPANDED + 1,
    );
    let mut archive = tar::Archive::new(decoded);
    let mut names = BTreeSet::new();
    let mut links = Vec::new();
    let mut expanded = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(normal(&path), "non-relative archive path");
        let relative = path
            .strip_prefix(&package.root)
            .context("archive has unexpected root")?;
        if relative.as_os_str().is_empty() {
            ensure!(
                entry.header().entry_type().is_dir(),
                "archive root is not a directory"
            );
            continue;
        }
        ensure!(
            normal(relative)
                && relative != Path::new(RECEIPT)
                && names.insert(relative.to_path_buf())
                && names.len() <= MAX_ENTRIES,
            "invalid, reserved, duplicate or excessive archive path"
        );
        expanded = expanded
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(expanded <= MAX_EXPANDED, "expanded archive exceeds limit");
        parents(stage, relative.parent().unwrap())?;
        let destination = stage.join(relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            private_dir(&destination, true)?;
        } else if kind.is_file() {
            let mode = if entry.header().mode()? & 0o111 != 0 {
                0o700
            } else {
                0o600
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(destination)?;
            let size = std::io::copy(&mut entry, &mut file)?;
            ensure!(size == entry.size(), "truncated archive member");
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.sync_all()?;
        } else if kind.is_symlink() {
            let target = entry
                .link_name()?
                .context("symlink target missing")?
                .into_owned();
            ensure!(
                normal(&target) && target.components().count() == 1,
                "archive symlink must reference a sibling file"
            );
            links.push((destination, target));
        } else {
            anyhow::bail!("unsupported archive entry type");
        }
    }
    // Create links last, so no extraction path can traverse an archive link.
    for (destination, target) in links {
        ensure!(
            fs::symlink_metadata(destination.parent().unwrap().join(&target))?.is_file(),
            "symlink target must be a regular sibling file"
        );
        symlink(target, destination)?;
    }
    for required in [
        "bin/postgres",
        "bin/initdb",
        "bin/pg_ctl",
        "bin/pg_controldata",
        "bin/pg_dump",
        "bin/pg_dumpall",
        "bin/pg_restore",
        "bin/psql",
        "share/extension/uuid-ossp.control",
        "share/extension/uuid-ossp--1.1.sql",
        "COPYRIGHT",
        "LICENSE",
    ] {
        ensure!(
            fs::symlink_metadata(stage.join(required))?.is_file(),
            "required package member missing: {required}"
        );
    }
    let extension = if package.target.ends_with("apple-darwin") {
        "lib/uuid-ossp.dylib"
    } else {
        "lib/uuid-ossp.so"
    };
    ensure!(
        fs::symlink_metadata(stage.join(extension))?.is_file(),
        "uuid-ossp library missing"
    );
    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
enum Member {
    Directory,
    File {
        sha256: String,
        bytes: u64,
        executable: bool,
    },
    Symlink {
        target: PathBuf,
    },
}

fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, Member>> {
    fn visit(
        root: &Path,
        path: &Path,
        map: &mut BTreeMap<PathBuf, Member>,
        bytes: &mut u64,
    ) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            let relative = path.strip_prefix(root)?;
            if relative == Path::new(RECEIPT) {
                continue;
            }
            ensure!(
                normal(relative) && map.len() < MAX_ENTRIES,
                "invalid installed package path"
            );
            let m = fs::symlink_metadata(&path)?;
            ensure!(
                m.uid() == unsafe { libc::geteuid() },
                "installed package owner changed"
            );
            let member = if m.is_dir() {
                private_dir(&path, false)?;
                Member::Directory
            } else if m.is_file() {
                ensure!(
                    m.mode() & 0o077 == 0 && m.nlink() == 1,
                    "installed file is not private or has hard links"
                );
                *bytes = bytes
                    .checked_add(m.len())
                    .context("installed size overflow")?;
                ensure!(
                    *bytes <= MAX_EXPANDED,
                    "installed package exceeds size limit"
                );
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&path)?;
                let mut contents = Vec::new();
                file.take(m.len() + 1).read_to_end(&mut contents)?;
                ensure!(
                    contents.len() as u64 == m.len(),
                    "installed file changed during read"
                );
                Member::File {
                    sha256: hash(&contents),
                    bytes: m.len(),
                    executable: m.mode() & 0o111 != 0,
                }
            } else if m.file_type().is_symlink() {
                let target = fs::read_link(&path)?;
                ensure!(
                    normal(&target) && target.components().count() == 1,
                    "installed symlink escapes package"
                );
                Member::Symlink { target }
            } else {
                anyhow::bail!("unsupported installed package file");
            };
            let directory = matches!(member, Member::Directory);
            map.insert(relative.to_path_buf(), member);
            if directory {
                visit(root, &path, map, bytes)?;
            }
        }
        Ok(())
    }
    let mut map = BTreeMap::new();
    visit(root, root, &mut map, &mut 0)?;
    Ok(map)
}

fn publish(stage: &Path, destination: &Path) -> Result<()> {
    let stage = std::ffi::CString::new(stage.as_os_str().as_bytes())?;
    let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(stage.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn install(base: &Path, package: &Package, bytes: &[u8]) -> Result<PathBuf> {
    verify_bytes(package, bytes)?; // Before creating any destination state.
    ensure!(base.is_absolute(), "absolute package directory required");
    private_dir(base, true)?;
    let base = base.canonicalize()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(base.join(".install.lock"))?;
    let m = lock.metadata()?;
    ensure!(
        m.is_file() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "invalid package lock"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.into()),
        }
    }
    let stage = base.join(format!(".install-{}", Uuid::new_v4()));
    private_dir(&stage, true)?;
    extract(bytes, package, &stage)
        .with_context(|| format!("package staging retained at {}", stage.display()))?;
    let mut notices = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("YGG-THIRD-PARTY-NOTICES.txt"))?;
    notices.write_all(b"PostgreSQL and theseus-rs distribution notices are retained in COPYRIGHT and LICENSE.\nThe macOS distributions bundle OpenSSL 3.6.3 from the OpenSSL Project.\nSource: https://github.com/openssl/openssl/tree/openssl-3.6.3\nOpenSSL is licensed under Apache License 2.0, reproduced below. Linux dynamically links the system libraries.\n\n")?;
    notices.write_all(include_bytes!("licenses/openssl-3.6.3-LICENSE.txt"))?;
    notices.sync_all()?;
    let mut provenance = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.join("YGG-RELEASE.json"))?;
    provenance.write_all(include_bytes!("packages.json"))?;
    provenance.sync_all()?;
    let expected = inventory(&stage)?;
    let receipt = serde_json::to_vec(&(package, &expected))?;
    let destination = base.join(&package.root);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            private_dir(&destination, false)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(destination.join(RECEIPT))?;
            ensure!(file.metadata()?.is_file(), "invalid package receipt");
            let mut existing = Vec::new();
            file.take(receipt.len() as u64 + 1)
                .read_to_end(&mut existing)?;
            ensure!(
                existing == receipt && inventory(&destination)? == expected,
                "installed package changed; refusing replacement"
            );
            fs::remove_dir_all(&stage)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(stage.join(RECEIPT))?;
            file.write_all(&receipt)?;
            file.sync_all()?;
            // Entries were individually synced; sync directories deepest-first.
            let mut dirs: Vec<_> = expected
                .iter()
                .filter(|(_, m)| matches!(m, Member::Directory))
                .map(|(p, _)| p)
                .collect();
            dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
            for dir in dirs {
                File::open(stage.join(dir))?.sync_all()?;
            }
            File::open(&stage)?.sync_all()?;
            publish(&stage, &destination)?;
        }
        Err(e) => return Err(e.into()),
    }
    File::open(base)?.sync_all()?;
    Ok(destination.join("bin"))
}

/// Install the exact native pinned archive. No network or subprocess execution.
pub fn install_offline(base: &Path, archive: &Path) -> Result<PathBuf> {
    let package = package(current_target()?)?;
    let bytes = archive_bytes(archive, &package)?;
    install(base, &package, &bytes)
}

/// Explicit online installation only; curl is not required for offline packages.
pub async fn install_download(base: &Path) -> Result<PathBuf> {
    let package = package(current_target()?)?;
    let mut child = Command::new("curl")
        .args([
            "-q",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "180",
            "--max-filesize",
        ])
        .arg(package.bytes.to_string())
        .arg(&package.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("online package install requires curl; use an offline archive otherwise")?;
    let mut bytes = Vec::new();
    let mut stdout = child
        .stdout
        .take()
        .context("download output unavailable")?
        .take(package.bytes.min(MAX_ARCHIVE) + 1);
    tokio::time::timeout(Duration::from_secs(185), stdout.read_to_end(&mut bytes))
        .await
        .context("package download timed out")??;
    verify_bytes(&package, &bytes)?;
    ensure!(child.wait().await?.success(), "package download failed");
    let base = base.to_path_buf();
    tokio::task::spawn_blocking(move || install(&base, &package, &bytes)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str, link: Option<&str>, hard: bool) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(0);
        // Write raw names to exercise rejection even when Builder::append_data
        // would normally prevent creating this malicious fixture.
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        if let Some(link) = link {
            header.set_entry_type(if hard {
                tar::EntryType::Link
            } else {
                tar::EntryType::Symlink
            });
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        archive.append(&header, std::io::empty()).unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extraction_rejects_escapes_and_special_links() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("stage");
        private_dir(&stage, true).unwrap();
        let p = Package {
            target: "aarch64-apple-darwin".into(),
            url: String::new(),
            sha256: String::new(),
            bytes: 0,
            root: "root".into(),
        };
        for (path, link, hard, error) in [
            ("../escape", None, false, "non-relative"),
            ("/escape", None, false, "non-relative"),
            ("root/../../escape", None, false, "non-relative"),
            ("elsewhere/file", None, false, "unexpected root"),
            ("root/out", Some("../escape"), false, "sibling"),
            ("root/out", Some("file"), true, "unsupported"),
        ] {
            assert!(
                extract(&fixture(path, link, hard), &p, &stage)
                    .unwrap_err()
                    .to_string()
                    .contains(error)
            );
            assert!(!temp.path().join("escape").exists());
        }
    }

    #[test]
    fn publication_never_replaces_an_existing_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("stage");
        let destination = temp.path().join("destination");
        private_dir(&stage, true).unwrap();
        private_dir(&destination, true).unwrap();
        fs::write(stage.join("retained"), "data").unwrap();
        assert!(publish(&stage, &destination).is_err());
        assert!(stage.join("retained").exists());
        assert!(fs::read_dir(&destination).unwrap().next().is_none());
    }
}
