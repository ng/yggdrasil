//! Short transient endpoints for long persistent cluster paths. Data, logs and
//! authoritative metadata remain under the configured data directory.
use anyhow::{Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

fn short(id: Uuid) -> Result<PathBuf> {
    // Do not follow caller-controlled TMPDIR. The platform /tmp alias is
    // canonicalized so PostgreSQL's PID file and our manifest agree.
    let parent = Path::new("/tmp").canonicalize()?;
    let meta = fs::metadata(&parent)?;
    ensure!(
        meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o1000 != 0,
        "short runtime parent must be root-owned and sticky"
    );
    Ok(parent.join(format!(
        "ygg-{}-{}",
        unsafe { libc::geteuid() },
        id.simple()
    )))
}
fn fits(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().len() + 15 < 104
}
pub(super) fn choose(root: &Path, id: Uuid) -> Result<Option<PathBuf>> {
    let legacy = root.join("runtime");
    // Avoid PostgreSQL's directory-list quoting rules for whitespace/control
    // characters and punctuation in arbitrary user-selected data paths.
    let simple = legacy.to_str().is_some_and(|s| {
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    });
    if fits(&legacy) && simple {
        Ok(None)
    } else {
        Ok(Some(short(id)?))
    }
}
pub(super) fn path(root: &Path, selected: Option<&Path>) -> PathBuf {
    selected
        .map(Path::to_owned)
        .unwrap_or_else(|| root.join("runtime"))
}
pub(super) fn validate_selection(root: &Path, id: Uuid, selected: Option<&Path>) -> Result<()> {
    let path = path(root, selected);
    ensure!(
        fits(&path) && path.to_str().is_some(),
        "managed socket path exceeds portable limit or is not UTF-8"
    );
    if let Some(selected) = selected {
        ensure!(
            selected == short(id)?,
            "short runtime endpoint differs from cluster identity"
        );
    }
    Ok(())
}

/// Missing short directories are permitted for read-only inspection. Only the
/// caller holding the persistent cluster lease may create/recover an endpoint,
/// and only after proving the server is stopped when the directory is missing.
pub(super) fn check(root: &Path, id: Uuid, selected: Option<&Path>, create: bool) -> Result<()> {
    validate_selection(root, id, selected)?;
    let path = path(root, selected);
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if !create && selected.is_some() && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "runtime endpoint must be a private owned directory"
    );
    if selected.is_none() {
        return Ok(());
    }
    let marker = path.join("cluster.json");
    let expected = serde_json::to_vec(&(root, id))?;
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&marker)
    {
        Ok(file) => {
            let m = file.metadata()?;
            ensure!(
                m.is_file()
                    && m.nlink() == 1
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.mode() & 0o077 == 0,
                "invalid runtime endpoint identity file"
            );
            let mut bytes = Vec::new();
            file.take(16385).read_to_end(&mut bytes)?;
            ensure!(
                bytes == expected,
                "runtime endpoint belongs to a different cluster"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            for entry in fs::read_dir(&path)? {
                let entry = entry?;
                let name = entry.file_name();
                let known = name
                    .to_str()
                    .and_then(|n| n.strip_prefix(".cluster-"))
                    .and_then(|n| n.strip_suffix(".tmp"))
                    .is_some_and(|id| Uuid::parse_str(id).is_ok());
                let m = fs::symlink_metadata(entry.path())?;
                ensure!(
                    known
                        && m.is_file()
                        && m.nlink() == 1
                        && m.uid() == unsafe { libc::geteuid() }
                        && m.mode() & 0o077 == 0,
                    "runtime endpoint has contents without an identity; refusing adoption"
                );
            }
            if create {
                let temporary = path.join(format!(".cluster-{}.tmp", Uuid::new_v4()));
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&temporary)?;
                file.write_all(&expected)?;
                file.sync_all()?;
                super::package::publish(&temporary, &marker)?;
                File::open(&path)?.sync_all()?;
                File::open(path.parent().unwrap())?.sync_all()?;
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_endpoint_is_private_identity_bound_and_read_only_when_absent() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        assert!(
            choose(Path::new("/short/path"), Uuid::new_v4())
                .unwrap()
                .is_none()
        );
        assert!(
            choose(
                Path::new("/short/path with spaces, punctuation"),
                Uuid::new_v4()
            )
            .unwrap()
            .is_some()
        );
        let root = temp.path().join("x".repeat(150));
        let id = Uuid::new_v4();
        let selected = choose(&root, id).unwrap().unwrap();
        check(&root, id, Some(&selected), false).unwrap();
        assert!(!selected.exists());
        check(&root, id, Some(&selected), true).unwrap();
        assert_eq!(fs::metadata(&selected).unwrap().mode() & 0o777, 0o700);
        assert!(check(&root, Uuid::new_v4(), Some(&selected), false).is_err());
        fs::remove_file(selected.join("cluster.json")).unwrap();
        let partial = selected.join(format!(".cluster-{}.tmp", Uuid::new_v4()));
        fs::write(&partial, "partial identity write").unwrap();
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).unwrap();
        check(&root, id, Some(&selected), false).unwrap();
        assert!(!selected.join("cluster.json").exists());
        check(&root, id, Some(&selected), true).unwrap();
        assert!(partial.exists(), "interrupted stage must be retained");
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check(&root, id, Some(&selected), false).is_err());
        fs::set_permissions(&selected, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(selected.join("cluster.json"), "wrong identity").unwrap();
        assert!(check(&root, id, Some(&selected), true).is_err());
        fs::remove_dir_all(&selected).unwrap();
        symlink(temp.path(), &selected).unwrap();
        assert!(check(&root, id, Some(&selected), false).is_err());
        fs::remove_file(&selected).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&selected).unwrap();
        fs::write(selected.join("unexpected"), "preserve").unwrap();
        assert!(check(&root, id, Some(&selected), true).is_err());
        fs::remove_dir_all(&selected).unwrap();
    }
}
