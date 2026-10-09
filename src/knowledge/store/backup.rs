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

/// Retained cooperative writer leases for a corpus and its separate policy.
/// Keep this value alive through the transaction that consumes the backups.
/// External editors must still be quiesced; `verify_sources` detects changes
/// before committing but cannot prevent an uncooperative later filesystem edit.
pub struct PairedBackup {
    corpus: KnowledgeBackup,
    policy: KnowledgeBackup,
    corpus_root: File,
    policy_root: File,
    _leases: Vec<File>,
}

impl PairedBackup {
    pub(crate) fn verify_paths(&self, corpus: &Path, policy: &Path) -> Result<()> {
        for (root, path) in [(&self.corpus_root, corpus), (&self.policy_root, policy)] {
            let expected = root.metadata()?;
            let actual = std::fs::symlink_metadata(path)?;
            ensure!(
                actual.is_dir() && actual.dev() == expected.dev() && actual.ino() == expected.ino(),
                "recovery source path changed"
            );
        }
        self.verify_sources()
    }

    pub(crate) fn require_outside_sources(&self, destination: &Path) -> Result<()> {
        let a = self.corpus_root.metadata()?;
        let b = self.policy_root.metadata()?;
        for ancestor in destination.ancestors() {
            let metadata = match std::fs::metadata(ancestor) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let identity = (metadata.dev(), metadata.ino());
            ensure!(
                identity != (a.dev(), a.ino()) && identity != (b.dev(), b.ino()),
                "rollback journal must be outside corpus and policy"
            );
        }
        Ok(())
    }

    pub(crate) fn verify_corpus_root(&self, source: &KnowledgeStore, path: &Path) -> Result<()> {
        let expected = self.corpus_root.metadata()?;
        for actual in [source.root.metadata()?, std::fs::symlink_metadata(path)?] {
            ensure!(
                actual.is_dir() && actual.dev() == expected.dev() && actual.ino() == expected.ino(),
                "recovery backup belongs to a different corpus root"
            );
        }
        self.verify_sources()
    }

    /// Publish the cutover selection while retaining both writer leases. Consume
    /// the guard because the policy intentionally ceases to match its old backup.
    pub(crate) fn publish_selection(
        self,
        operation: Uuid,
        expected: &str,
        selected: &str,
    ) -> Result<()> {
        self.verify_sources()?;
        let name = crate::knowledge::runtime::SELECTION_FILE;
        ensure!(
            KnowledgeStore::read_at(&self.policy_root, name)?.as_deref() == Some(expected),
            "selection changed before activation"
        );
        ensure!(
            selected.len() <= MAX_DOCUMENT_BYTES,
            "selection exceeds byte limit"
        );
        let temporary = format!(".cutover-{operation}.tmp");
        ensure!(
            !self.policy.entries.contains_key(&temporary),
            "cutover temporary name conflicts with retained policy"
        );
        // The durable journal names this file before it can be created. A kill
        // during write therefore leaves identifiable, bounded recovery evidence.
        let mut file = child(
            &self.policy_root,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        file.write_all(selected.as_bytes())?;
        file.sync_all()?;
        let old = CString::new(temporary)?;
        let new = CString::new(name)?;
        if unsafe {
            libc::renameat(
                self.policy_root.as_raw_fd(),
                old.as_ptr(),
                self.policy_root.as_raw_fd(),
                new.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        self.policy_root.sync_all()?;
        for (root, mut expected_entries, policy) in [
            (&self.corpus_root, self.corpus.entries.clone(), false),
            (&self.policy_root, self.policy.entries.clone(), true),
        ] {
            if policy {
                expected_entries.insert(
                    name.into(),
                    BackupEntry::File {
                        bytes: selected.len() as u64,
                        sha256: digest(selected.as_bytes()),
                    },
                );
            }
            let mut actual = BTreeMap::new();
            inventory(root, None, "", 0, true, &mut actual, &mut 0)?;
            ensure!(
                actual == expected_entries,
                "source changed during selection publication"
            );
        }
        Ok(())
    }

    fn recover_selection_temporary(&self, operation: Uuid, selected: &str) -> Result<()> {
        let name = format!(".cutover-{operation}.tmp");
        ensure!(
            !self.policy.entries.contains_key(&name),
            "cutover temporary name conflicts with retained policy"
        );
        let file = match child(&self.policy_root, &name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.verify_sources();
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= selected.len() as u64,
            "invalid interrupted selection temporary"
        );
        let mut partial = Vec::new();
        file.take(selected.len() as u64 + 1)
            .read_to_end(&mut partial)?;
        ensure!(
            selected.as_bytes().starts_with(&partial),
            "interrupted selection temporary conflicts with intent"
        );
        // Verify all other bytes before removing even this named, matching prefix.
        for (root, expected, policy) in [
            (&self.corpus_root, &self.corpus.entries, false),
            (&self.policy_root, &self.policy.entries, true),
        ] {
            let mut actual = BTreeMap::new();
            inventory(root, None, "", 0, true, &mut actual, &mut 0)?;
            if policy {
                actual.remove(&name);
            }
            ensure!(
                &actual == expected,
                "source changed beside interrupted selection temporary"
            );
        }
        unlink(&self.policy_root, &name)?;
        self.policy_root.sync_all()?;
        self.verify_sources()
    }

    pub fn corpus(&self) -> &KnowledgeBackup {
        &self.corpus
    }
    pub fn policy(&self) -> &KnowledgeBackup {
        &self.policy
    }

    /// Parse the retained source roots without reacquiring their writer locks.
    /// This is the private corpus snapshot; shared Git callers must resolve and
    /// pin their authoritative confirmed tree separately, not use its cache root.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.verify_sources()?;
        ensure!(
            KnowledgeStore::read_at(&self.corpus_root, ".shared-mode.json")?.is_none(),
            "shared recovery requires the pinned authoritative Git tree"
        );
        let source = KnowledgeStore {
            root: self.corpus_root.try_clone()?,
        };
        let snapshot = source.snapshot_under_lease();
        source.require_representable_snapshot(&snapshot)?;
        ensure!(
            snapshot.diagnostics.is_empty(),
            "incomplete recovery corpus: {:?}",
            snapshot.diagnostics
        );
        self.verify_sources()?;
        Ok(snapshot)
    }

    /// Revalidate exact current inventories using the originally opened roots.
    /// Does not reacquire locks or modify the source or retained archives.
    pub fn verify_sources(&self) -> Result<()> {
        for (root, expected) in [
            (&self.corpus_root, &self.corpus),
            (&self.policy_root, &self.policy),
        ] {
            let mut entries = BTreeMap::new();
            inventory(root, None, "", 0, true, &mut entries, &mut 0)?;
            ensure!(
                entries == expected.entries,
                "knowledge or policy changed after recovery capture"
            );
        }
        Ok(())
    }
}

fn skipped(name: &str, root: bool, shared: bool) -> bool {
    if !root {
        return false;
    }
    if matches!(
        name,
        ".writer.lock"
            | ".export.lock"
            | ".selection.lock"
            | ".shared.lock"
            | ".lookup.json"
            | ".lookup-notes.json"
            | ".lookup-rules.json"
            | ".sessions"
    ) {
        return true;
    }
    shared
        && (name
            .strip_prefix(".view-")
            .or_else(|| name.strip_prefix(".init-"))
            .is_some_and(|s| Uuid::parse_str(s).is_ok())
            || name
                .strip_prefix('.')
                .and_then(|s| {
                    s.strip_suffix(".tmp")
                        .or_else(|| s.strip_suffix(".tmp.lock"))
                })
                .is_some_and(|s| Uuid::parse_str(s).is_ok()))
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
    let shared =
        depth == 0 && skip_cache && KnowledgeStore::read_at(source, ".shared-mode.json")?.is_some();
    for name in names_limited(source, MAX_ENTRIES.saturating_sub(entries.len()) + 3)? {
        if skip_cache && skipped(&name, depth == 0, shared) {
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
    fn pair_leases(&self, other: &Self) -> Result<Vec<File>> {
        let a = self.root.metadata()?;
        let b = other.root.metadata()?;
        let a = (a.dev(), a.ino());
        let b = (b.dev(), b.ino());
        ensure!(a != b, "bundle and policy must be separate directories");
        let (first, second) = if a < b { (self, other) } else { (other, self) };
        let mut leases = Vec::new();
        leases.extend(first.shared_backup_lease()?);
        leases.extend(second.shared_backup_lease()?);
        leases.push(first.operation_lock(".export.lock")?);
        leases.push(first.lock()?);
        leases.push(second.operation_lock(".export.lock")?);
        leases.push(second.lock()?);
        Ok(leases)
    }

    /// Reacquire recovery leases and verify existing immutable archives against
    /// both current source trees. Never recopy or replace recovery evidence.
    pub fn resume_pair_retained(
        &self,
        other: &Self,
        archive: &Path,
        policy_archive: &Path,
    ) -> Result<PairedBackup> {
        let leases = self.pair_leases(other)?;
        let saved = PairedBackup {
            corpus: KnowledgeBackup::verify(archive)?,
            policy: KnowledgeBackup::verify(policy_archive)?,
            corpus_root: self.root.try_clone()?,
            policy_root: other.root.try_clone()?,
            _leases: leases,
        };
        saved.verify_sources()?;
        Ok(saved)
    }

    /// Resume only the deterministic temporary named by an already durable
    /// cutover intent. Never ignore arbitrary temporary or independently edited files.
    pub(crate) fn resume_selection_pair(
        &self,
        other: &Self,
        archive: &Path,
        policy_archive: &Path,
        operation: Uuid,
        selected: &str,
    ) -> Result<PairedBackup> {
        let leases = self.pair_leases(other)?;
        let saved = PairedBackup {
            corpus: KnowledgeBackup::verify(archive)?,
            policy: KnowledgeBackup::verify(policy_archive)?,
            corpus_root: self.root.try_clone()?,
            policy_root: other.root.try_clone()?,
            _leases: leases,
        };
        saved.recover_selection_temporary(operation, selected)?;
        Ok(saved)
    }

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
        let _shared = self.shared_backup_lease()?;
        let _export = self.operation_lock(".export.lock")?;
        let _writer = self.lock()?;
        self.backup_under_leases(destination, checkpoint)
    }

    fn shared_backup_lease(&self) -> Result<Option<File>> {
        if self.read_control(".shared-mode.json")?.is_some() {
            Ok(Some(self.bounded_lock(".shared.lock")?))
        } else {
            Ok(None)
        }
    }

    /// Snapshot a bundle and its separate policy directory under both leases.
    pub(crate) fn backup_pair(
        &self,
        other: &Self,
        destination: &Path,
        other_destination: &Path,
    ) -> Result<(KnowledgeBackup, KnowledgeBackup)> {
        let saved = self.backup_pair_retained(other, destination, other_destination)?;
        Ok((saved.corpus.clone(), saved.policy.clone()))
    }

    /// Capture both trees and retain all cooperative writer/export/shared leases.
    /// Acquire any selection lease before this call, then acquire database leases
    /// afterward. Destination publication remains exclusive; failed captures are
    /// retained and never authorize applying SQL or selecting a storage backend.
    pub fn backup_pair_retained(
        &self,
        other: &Self,
        destination: &Path,
        other_destination: &Path,
    ) -> Result<PairedBackup> {
        self.capture_pair(other, destination, other_destination, false)
    }

    /// Complete an interrupted pair without replacing either published archive.
    /// Every retained archive must still match its corresponding source exactly.
    pub(crate) fn complete_pair_retained(
        &self,
        other: &Self,
        destination: &Path,
        other_destination: &Path,
    ) -> Result<PairedBackup> {
        self.capture_pair(other, destination, other_destination, true)
    }

    fn capture_pair(
        &self,
        other: &Self,
        destination: &Path,
        other_destination: &Path,
        resume: bool,
    ) -> Result<PairedBackup> {
        let a = self.root.metadata()?;
        let b = other.root.metadata()?;
        let a = (a.dev(), a.ino());
        let b = (b.dev(), b.ino());
        ensure!(a != b, "bundle and policy must be separate directories");
        let mut targets = Vec::new();
        for path in [destination, other_destination] {
            ensure!(path.is_absolute(), "absolute backup destinations required");
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("backup parent required"))?
                .canonicalize()?;
            for ancestor in parent.ancestors() {
                let metadata = std::fs::metadata(ancestor)?;
                let identity = (metadata.dev(), metadata.ino());
                ensure!(
                    identity != a && identity != b,
                    "paired backup destinations must be outside both sources"
                );
            }
            targets.push(
                parent.join(
                    path.file_name()
                        .ok_or_else(|| anyhow::anyhow!("backup filename required"))?,
                ),
            );
        }
        ensure!(
            !targets[0].starts_with(&targets[1]) && !targets[1].starts_with(&targets[0]),
            "paired backup destinations must be separate"
        );
        let leases = self.pair_leases(other)?;
        let capture = |source: &Self, path: &Path| -> Result<KnowledgeBackup> {
            match std::fs::symlink_metadata(path) {
                Ok(_) if resume => KnowledgeBackup::verify(path),
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => source.backup_under_leases(path, &|_| {}),
            }
        };
        let saved = PairedBackup {
            corpus: capture(self, destination)?,
            policy: capture(other, other_destination)?,
            corpus_root: self.root.try_clone()?,
            policy_root: other.root.try_clone()?,
            _leases: leases,
        };
        // The second copy can take time; recheck the first tree as well before
        // acknowledging the pair, while retaining both trees' leases.
        saved.verify_sources()?;
        Ok(saved)
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

    /// Permit only the independently derived runtime binding bytes to differ.
    pub(crate) fn verify_restored_rebased_selection(
        path: &Path,
        restored: &Path,
        selection: &str,
    ) -> Result<()> {
        let mut expected = Self::verify(path)?;
        let name = crate::knowledge::runtime::SELECTION_FILE;
        ensure!(
            expected.entries.contains_key(name),
            "backup has no selection to rebase"
        );
        expected.entries.insert(
            name.to_owned(),
            BackupEntry::File {
                bytes: selection.len() as u64,
                sha256: crate::knowledge::document::digest(selection.as_bytes()),
            },
        );
        let restored = KnowledgeStore::open(restored, false)?;
        let mut entries = BTreeMap::new();
        inventory(&restored.root, None, "", 0, true, &mut entries, &mut 0)?;
        ensure!(
            entries == expected.entries,
            "restored policy differs from backup and derived selection binding"
        );
        Ok(())
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
    fn complete_partial_pair_preserves_archives_and_refuses_changed_sources() {
        for changed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let corpus_path = temp.path().join("corpus");
            let policy_path = temp.path().join("policy");
            let corpus = KnowledgeStore::open(&corpus_path, true).unwrap();
            let policy = KnowledgeStore::open(&policy_path, true).unwrap();
            std::fs::write(corpus_path.join("retained"), "original bytes").unwrap();
            std::fs::write(policy_path.join("identity"), "explicit policy").unwrap();
            let a = temp.path().join("a");
            let b = temp.path().join("b");
            let original = corpus.backup(&a).unwrap();
            if changed {
                std::fs::write(corpus_path.join("retained"), "independent edit").unwrap();
            }
            let result = corpus.complete_pair_retained(&policy, &a, &b);
            assert_eq!(result.is_err(), changed);
            drop(result);
            assert_eq!(
                KnowledgeBackup::verify(&a).unwrap().revision,
                original.revision
            );
            if !changed {
                let pair = corpus.complete_pair_retained(&policy, &a, &b).unwrap();
                pair.verify_sources().unwrap();
                assert!(corpus.try_export_lease().is_err());
                assert!(policy.try_export_lease().is_err());
            }
        }
    }

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
