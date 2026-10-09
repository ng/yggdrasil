//! Complete, bounded corpus snapshots. This is the knowledge component of a
//! deployment backup, not a replacement for a consistent PostgreSQL dump.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};

const MAX_ENTRIES: usize = 100_000;
const MAX_FILE: u64 = 1024 * 1024 * 1024;
const MAX_TOTAL: u64 = 4 * MAX_FILE;
const MANIFEST: &str = "knowledge-backup.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackupEntry {
    Directory,
    File { bytes: u64, sha256: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeBackup {
    pub version: u32,
    pub revision: String,
    pub entries: BTreeMap<String, BackupEntry>,
}

fn skipped(name: &str, root: bool) -> bool {
    root && matches!(
        name,
        ".writer.lock" | ".export.lock" | ".selection.lock" | ".lookup.json" | ".sessions"
    )
}

fn inventory(
    source: &File,
    target: Option<&File>,
    prefix: &str,
    depth: usize,
    skip_cache: bool,
    entries: &mut BTreeMap<String, BackupEntry>,
    total: &mut u64,
) -> Result<()> {
    ensure!(
        depth <= 32,
        "knowledge backup exceeds directory depth limit"
    );
    for name in names_limited(source, MAX_ENTRIES.saturating_sub(entries.len()) + 3)? {
        if skip_cache && skipped(&name, depth == 0) {
            continue;
        }
        ensure!(
            entries.len() < MAX_ENTRIES,
            "knowledge backup exceeds entry limit"
        );
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let mut input = child(source, &name, libc::O_RDONLY, 0)?;
        let metadata = input.metadata()?;
        if metadata.is_dir() {
            entries.insert(path.clone(), BackupEntry::Directory);
            let output = target
                .map(|target| directory(target, &name, true))
                .transpose()?;
            inventory(
                &input,
                output.as_ref(),
                &path,
                depth + 1,
                skip_cache,
                entries,
                total,
            )?;
            if let Some(output) = output {
                output.sync_all()?;
            }
        } else {
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "knowledge backup requires regular, non-hardlinked files"
            );
            ensure!(
                metadata.len() <= MAX_FILE,
                "knowledge backup file exceeds size limit"
            );
            let mut output = target
                .map(|target| {
                    child(
                        target,
                        &name,
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                        0o600,
                    )
                })
                .transpose()?;
            let mut hash = Sha256::new();
            let mut bytes = 0;
            let mut buffer = [0u8; 65536];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                bytes += count as u64;
                *total += count as u64;
                ensure!(
                    bytes <= MAX_FILE && *total <= MAX_TOTAL,
                    "knowledge backup exceeds byte limit"
                );
                hash.update(&buffer[..count]);
                if let Some(output) = &mut output {
                    output.write_all(&buffer[..count])?;
                }
            }
            ensure!(
                bytes == metadata.len(),
                "knowledge file changed during backup"
            );
            if let Some(output) = output {
                output.sync_all()?;
            }
            entries.insert(
                path,
                BackupEntry::File {
                    bytes,
                    sha256: hash
                        .finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                },
            );
        }
    }
    Ok(())
}

fn publish(parent: &File, stage: &str, name: &str) -> Result<()> {
    let stage = CString::new(stage)?;
    let name = CString::new(name)?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            stage.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            stage.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    ensure!(
        result == 0,
        "cannot publish knowledge backup: {}",
        std::io::Error::last_os_error()
    );
    parent.sync_all()?;
    Ok(())
}

impl KnowledgeStore {
    /// Snapshot exact bytes, unknown files, empty directories and policy/identity
    /// data. Never overwrite a destination. Retain incomplete private staging on
    /// failure. Cooperative writers/exporters are excluded; external editors must
    /// be quiesced for a guaranteed point-in-time corpus snapshot.
    pub fn backup(&self, destination: &Path) -> Result<KnowledgeBackup> {
        self.backup_with_checkpoint(destination, &|_| {})
    }

    fn backup_with_checkpoint(
        &self,
        destination: &Path,
        checkpoint: &dyn Fn(&str),
    ) -> Result<KnowledgeBackup> {
        let _export = self.operation_lock(".export.lock")?;
        let _writer = self.lock()?;
        self.backup_under_leases(destination, checkpoint)
    }

    /// Snapshot a bundle and its separate policy directory under both leases.
    pub(crate) fn backup_pair(
        &self,
        other: &Self,
        destination: &Path,
        other_destination: &Path,
    ) -> Result<(KnowledgeBackup, KnowledgeBackup)> {
        let a = self.root.metadata()?;
        let b = other.root.metadata()?;
        let a = (a.dev(), a.ino());
        let b = (b.dev(), b.ino());
        ensure!(a != b, "bundle and policy must be separate directories");
        let (first, second) = if a < b { (self, other) } else { (other, self) };
        let _export_a = first.operation_lock(".export.lock")?;
        let _writer_a = first.lock()?;
        let _export_b = second.operation_lock(".export.lock")?;
        let _writer_b = second.lock()?;
        Ok((
            self.backup_under_leases(destination, &|_| {})?,
            other.backup_under_leases(other_destination, &|_| {})?,
        ))
    }

    fn backup_under_leases(
        &self,
        destination: &Path,
        checkpoint: &dyn Fn(&str),
    ) -> Result<KnowledgeBackup> {
        ensure!(
            destination.is_absolute(),
            "absolute backup destination required"
        );
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid backup name"))?;
        let parent_path = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("backup parent required"))?
            .canonicalize()?;
        let source_metadata = self.root.metadata()?;
        for ancestor in parent_path.ancestors() {
            let metadata = std::fs::metadata(ancestor)?;
            ensure!(
                metadata.dev() != source_metadata.dev() || metadata.ino() != source_metadata.ino(),
                "backup destination cannot be inside source corpus"
            );
        }
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent_path)?;
        self.recover_move_locked()?;
        let stage_name = format!(".knowledge-backup-{}", Uuid::new_v4());
        let stage = directory(&parent, &stage_name, true)?;
        let corpus = directory(&stage, "corpus", true)?;
        let mut entries = BTreeMap::new();
        inventory(&self.root, Some(&corpus), "", 0, true, &mut entries, &mut 0)?;
        checkpoint("copied");
        let mut current = BTreeMap::new();
        inventory(&self.root, None, "", 0, true, &mut current, &mut 0)?;
        ensure!(
            current == entries,
            "knowledge corpus changed during backup; quiesce external editors"
        );
        corpus.sync_all()?;
        let manifest = KnowledgeBackup {
            version: 1,
            revision: digest(&serde_json::to_vec(&entries)?),
            entries,
        };
        let mut file = child(
            &stage,
            MANIFEST,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let encoded = serde_json::to_vec_pretty(&manifest)?;
        ensure!(
            encoded.len() <= 32 * 1024 * 1024,
            "backup manifest exceeds size limit"
        );
        file.write_all(&encoded)?;
        file.sync_all()?;
        stage.sync_all()?;
        checkpoint("manifest");
        publish(&parent, &stage_name, name)?;
        checkpoint("published");
        Ok(manifest)
    }
}

impl KnowledgeBackup {
    /// Compare a restored live directory with the backup; disposable root cache
    /// and leases are excluded just as during capture. Never writes either tree.
    pub fn verify_restored(path: &Path, restored: &Path) -> Result<Self> {
        let expected = Self::verify(path)?;
        let restored = KnowledgeStore::open(restored, false)?;
        let mut entries = BTreeMap::new();
        inventory(&restored.root, None, "", 0, true, &mut entries, &mut 0)?;
        ensure!(
            entries == expected.entries,
            "restored knowledge or policy differs from backup"
        );
        Ok(expected)
    }

    /// Restore exact bytes into an absent directory. Never changes trust or
    /// overwrites an existing corpus. Caller authorizes the backed-up policy.
    pub fn restore(path: &Path, destination: &Path) -> Result<Self> {
        let expected = Self::verify(path)?;
        ensure!(
            destination.is_absolute(),
            "absolute restore destination required"
        );
        let parent_path = destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("restore parent required"))?
            .canonicalize()?;
        ensure!(
            !parent_path.starts_with(path.canonicalize()?),
            "restore destination cannot be inside backup"
        );
        let name = destination
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid restore name"))?;
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent_path)?;
        let metadata = parent.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "restore parent must be owned and not writable by others"
        );
        let source = KnowledgeStore::open(path, false)?;
        let corpus = directory(&source.root, "corpus", false)?;
        let stage_name = format!(".knowledge-restore-{}", Uuid::new_v4());
        let stage = directory(&parent, &stage_name, true)?;
        let mut entries = BTreeMap::new();
        inventory(&corpus, Some(&stage), "", 0, false, &mut entries, &mut 0)?;
        ensure!(
            entries == expected.entries,
            "knowledge backup changed during restore"
        );
        stage.sync_all()?;
        publish(&parent, &stage_name, name)?;
        Ok(expected)
    }

    /// Validate a trusted backup against its exact inventory. This checksum is
    /// integrity evidence, not a signature or authorization to trust imported rules.
    pub fn verify(path: &Path) -> Result<Self> {
        let root = KnowledgeStore::open(path, false)?;
        ensure!(
            names_limited(&root.root, 2)? == ["corpus", MANIFEST],
            "unexpected knowledge backup contents"
        );
        let mut file = child(&root.root, MANIFEST, libc::O_RDONLY, 0)?;
        ensure!(
            file.metadata()?.is_file(),
            "backup manifest must be regular"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 32 * 1024 * 1024,
            "backup manifest exceeds size limit"
        );
        let manifest: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            manifest.version == 1,
            "unsupported knowledge backup version"
        );
        let corpus = directory(&root.root, "corpus", false)?;
        let mut entries = BTreeMap::new();
        inventory(&corpus, None, "", 0, false, &mut entries, &mut 0)?;
        ensure!(
            manifest.entries == entries
                && manifest.revision == digest(&serde_json::to_vec(&entries)?),
            "knowledge backup integrity mismatch"
        );
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_source_refuses_publication_and_holds_writer_and_export_leases() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let store = KnowledgeStore::open(&source, true).unwrap();
        std::fs::write(source.join("file"), "before").unwrap();
        let target = temp.path().join("backup");
        let result = store.backup_with_checkpoint(&target, &|point| {
            if point == "copied" {
                for lock in [".writer.lock", ".export.lock"] {
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(source.join(lock))
                        .unwrap();
                    assert!(file.try_lock_exclusive().is_err());
                }
                std::fs::write(source.join("file"), "after").unwrap();
            }
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("changed during backup")
        );
        assert!(!target.exists());
        store.backup(&target).unwrap();
        KnowledgeBackup::verify(&target).unwrap();
    }

    #[test]
    fn crash_helper() {
        let Ok(root) = std::env::var("YGG_BACKUP_CRASH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let point = std::env::var("YGG_BACKUP_CRASH_POINT").unwrap();
        KnowledgeStore::open(&root.join("source"), false)
            .unwrap()
            .backup_with_checkpoint(&root.join("backup"), &|here| {
                if point == here {
                    std::fs::write(root.join("ready"), here).unwrap();
                    loop {
                        std::thread::park();
                    }
                }
            })
            .unwrap();
    }

    #[test]
    fn killed_backup_is_absent_or_complete_and_can_be_retried() {
        for point in ["copied", "manifest", "published"] {
            let temp = tempfile::tempdir().unwrap();
            let store = KnowledgeStore::open(&temp.path().join("source"), true).unwrap();
            std::fs::write(temp.path().join("source/file"), "durable bytes").unwrap();
            let mut process = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "knowledge::store::backup::tests::crash_helper",
                    "--nocapture",
                ])
                .env("YGG_BACKUP_CRASH_ROOT", temp.path())
                .env("YGG_BACKUP_CRASH_POINT", point)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !temp.path().join("ready").exists() {
                if std::time::Instant::now() >= deadline {
                    let _ = process.kill();
                    let _ = process.wait();
                    panic!("backup checkpoint timed out");
                }
                assert!(
                    process.try_wait().unwrap().is_none(),
                    "backup child exited early"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            process.kill().unwrap();
            process.wait().unwrap();
            let target = temp.path().join("backup");
            if point != "published" {
                assert!(!target.exists());
                store.backup(&target).unwrap();
            }
            KnowledgeBackup::verify(&target).unwrap();
            assert_eq!(
                std::fs::read(target.join("corpus/file")).unwrap(),
                b"durable bytes"
            );
        }
    }
}
