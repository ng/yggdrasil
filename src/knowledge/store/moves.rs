//! A single durable move intent serializes scope changes under the corpus lock.
//! Recovery finishes forward, verifying both revisions before deleting anything.
use super::*;
use crate::knowledge::document::State;

const JOURNAL: &str = ".move.json";
const JOURNAL_LIMIT: usize = MAX_DOCUMENT_BYTES + 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveIntent {
    version: u32,
    source: Key,
    target: Key,
    before: String,
    after: String,
}

impl KnowledgeStore {
    /// Preserve UUID and kind while moving between scopes. Rules must lose
    /// activation on a scope change. Success means target + source removal and
    /// intent removal are synced. An interrupted/failed call may finish on the
    /// next read or mutation; reload before retrying an ambiguous outcome.
    pub fn move_document(
        &self,
        source: Key,
        document: &Document,
        expected: &str,
    ) -> Result<RevisionedDocument> {
        self.move_with_checkpoint(source, document, expected, &|_| {})
    }

    fn move_with_checkpoint(
        &self,
        source: Key,
        document: &Document,
        expected: &str,
        checkpoint: &dyn Fn(&str),
    ) -> Result<RevisionedDocument> {
        let target = Key::from_document(document)?;
        ensure!(
            source.id == target.id && source.kind == target.kind && source.repo != target.repo,
            "move must preserve document UUID/type and change scope"
        );
        if target.kind == Kind::Learning {
            let profile = document.profile()?.unwrap();
            ensure!(
                profile.state == State::Pending && profile.approval.is_none(),
                "scope move requires fresh learning approval"
            );
        }
        let text = document.serialize()?;
        Document::parse(&text)?;
        let _lock = self.lock()?;
        self.recover_move_locked()?;
        self.check_unique(source)?;
        let source_dir = self.parent(source, false)?;
        let name = format!("{}.md", source.id);
        let before = Self::read_at(&source_dir, &name)?
            .ok_or_else(|| anyhow::anyhow!("move source is missing"))?;
        Self::check_revision(Some(&before), ExpectedRevision::Digest(expected))?;
        ensure!(
            Key::from_document(&Document::parse(&before)?)? == source,
            "source identity does not match path"
        );
        let target_dir = self.parent(target, true)?;
        ensure!(
            Self::read_at(&target_dir, &name)?.is_none(),
            "move destination already exists"
        );
        let intent = MoveIntent {
            version: 1,
            source,
            target,
            before: expected.into(),
            after: digest(text.as_bytes()),
        };
        let journal = format!("{}\n{text}", serde_json::to_string(&intent)?);
        ensure!(
            journal.len() <= JOURNAL_LIMIT,
            "move intent exceeds byte limit"
        );
        Self::replace_at(&self.root, JOURNAL, &journal)?;
        checkpoint("intent");
        self.recover_with_checkpoint(checkpoint)?;
        Ok(RevisionedDocument {
            key: target,
            revision: intent.after,
            document: document.clone(),
        })
    }

    /// A complete staged inode is linked exclusively into the target name.
    /// Unlike rename-over-existing, this cannot overwrite an intervening file.
    fn publish_new(parent: &File, name: &str, text: &str) -> Result<()> {
        let temporary = format!(".{}.tmp", Uuid::new_v4());
        let result = (|| -> Result<()> {
            let mut file = child(
                parent,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            let old = CString::new(temporary.as_str())?;
            let new = CString::new(name)?;
            if unsafe {
                libc::linkat(
                    parent.as_raw_fd(),
                    old.as_ptr(),
                    parent.as_raw_fd(),
                    new.as_ptr(),
                    0,
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            parent.sync_all()?;
            unlink(parent, &temporary)?;
            parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlink(parent, &temporary);
        }
        result
    }

    fn read_move(&self) -> Result<Option<(MoveIntent, String)>> {
        let Some(text) = Self::read_limited(&self.root, JOURNAL, JOURNAL_LIMIT)? else {
            return Ok(None);
        };
        let (header, body) = text
            .split_once('\n')
            .ok_or_else(|| anyhow::anyhow!("incomplete move intent"))?;
        ensure!(header.len() <= 4096, "move header exceeds limit");
        let intent: MoveIntent = serde_json::from_str(header)?;
        ensure!(intent.version == 1, "unsupported move intent version");
        ensure!(
            intent.source.id == intent.target.id
                && intent.source.kind == intent.target.kind
                && intent.source.repo != intent.target.repo,
            "invalid move identity"
        );
        ensure!(
            intent.before.len() == 64 && intent.before.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source revision"
        );
        ensure!(
            digest(body.as_bytes()) == intent.after,
            "move content digest mismatch"
        );
        let document = Document::parse(body)?;
        ensure!(
            Key::from_document(&document)? == intent.target,
            "move content identity mismatch"
        );
        if intent.target.kind == Kind::Learning {
            let profile = document.profile()?.unwrap();
            ensure!(
                profile.state == State::Pending && profile.approval.is_none(),
                "move intent cannot preserve activation"
            );
        }
        Ok(Some((intent, body.into())))
    }

    pub(super) fn recover_move(&self) -> Result<()> {
        if self.read_move()?.is_none() {
            return Ok(());
        }
        let _lock = self.lock()?;
        self.recover_move_locked()
    }

    pub(super) fn recover_move_locked(&self) -> Result<()> {
        self.recover_with_checkpoint(&|_| {})
    }

    fn recover_with_checkpoint(&self, checkpoint: &dyn Fn(&str)) -> Result<()> {
        let Some((intent, text)) = self.read_move()? else {
            return Ok(());
        };
        let inventory = self.inventory();
        ensure!(
            !inventory.incomplete,
            "cannot recover move with incomplete inventory"
        );
        ensure!(
            inventory
                .keys
                .iter()
                .all(|k| k.id != intent.source.id || *k == intent.source || *k == intent.target),
            "move UUID found in an unrelated location"
        );
        let source = match self.parent(intent.source, false) {
            Ok(dir) => Some(dir),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(e) => return Err(e),
        };
        let target = self.parent(intent.target, true)?;
        let name = format!("{}.md", intent.source.id);
        let before = match &source {
            Some(dir) => Self::read_at(dir, &name)?,
            None => None,
        };
        let after = Self::read_at(&target, &name)?;
        if let Some(before) = &before {
            ensure!(
                digest(before.as_bytes()) == intent.before,
                "move source changed independently; manual resolution required"
            );
        }
        if let Some(after) = &after {
            ensure!(
                digest(after.as_bytes()) == intent.after,
                "move destination changed independently; manual resolution required"
            );
        }
        ensure!(
            before.is_some() || after.is_some(),
            "both move locations missing; restore from backup"
        );
        if after.is_none() {
            Self::publish_new(&target, &name, &text)?;
        }
        // Re-sync a target recovered after a crash before its original directory
        // sync. Never remove the durable source based only on a visible name.
        let target_file = child(&target, &name, libc::O_RDONLY, 0)?;
        target_file.sync_all()?;
        target.sync_all()?;
        checkpoint("destination");
        if before.is_some() {
            let source = source
                .as_ref()
                .expect("source bytes require source directory");
            Self::check_revision(
                Self::read_at(source, &name)?.as_deref(),
                ExpectedRevision::Digest(&intent.before),
            )?;
            unlink(source, &name)?;
        }
        if let Some(source) = source {
            source.sync_all()?;
        }
        checkpoint("source_removed");
        unlink(&self.root, JOURNAL)?;
        self.root.sync_all()?;
        checkpoint("journal_removed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> Document {
        Document::parse(include_str!("../../../tests/fixtures/knowledge/rule.md")).unwrap()
    }

    #[test]
    fn crash_writer() {
        let Some(root) = std::env::var_os("YGG_MOVE_CRASH_ROOT") else {
            return;
        };
        let stage = std::env::var("YGG_MOVE_CRASH_STAGE").unwrap();
        let store = KnowledgeStore::open(Path::new(&root), false).unwrap();
        let original = store
            .find(document().profile().unwrap().unwrap().id)
            .unwrap()
            .unwrap();
        let mut target = original.document.clone();
        let mut profile = target.profile().unwrap().unwrap();
        profile.repo = None;
        profile.scope = Scope::Global;
        target.set_profile(&profile).unwrap();
        store
            .move_with_checkpoint(original.key, &target, &original.revision, &|at| {
                if at == stage {
                    std::process::exit(86);
                }
            })
            .unwrap();
        panic!("crash checkpoint not reached");
    }

    #[test]
    fn abrupt_exit_at_each_durable_boundary_recovers_one_complete_document() {
        for stage in ["intent", "destination", "source_removed", "journal_removed"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("bundle");
            let store = KnowledgeStore::open(&root, true).unwrap();
            let original = store.put(&document(), ExpectedRevision::Absent).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "knowledge::store::moves::tests::crash_writer",
                    "--nocapture",
                ])
                .env("YGG_MOVE_CRASH_ROOT", &root)
                .env("YGG_MOVE_CRASH_STAGE", stage)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{stage}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if stage == "source_removed" {
                // Empty-directory cleanup after the source unlink is safe too.
                std::fs::remove_dir(root.join(original.key.relative_path()).parent().unwrap())
                    .unwrap();
            }
            let reopened = KnowledgeStore::open(&root, false).unwrap();
            let recovered = reopened.find(original.key.id).unwrap().unwrap();
            assert_eq!(recovered.key.repo, None, "{stage}");
            assert_eq!(recovered.document.body, original.document.body, "{stage}");
            assert!(reopened.get(original.key).unwrap().is_none(), "{stage}");
            let snapshot = reopened.snapshot();
            assert!(
                snapshot.diagnostics.is_empty(),
                "{stage}: {:?}",
                snapshot.diagnostics
            );
            assert_eq!(snapshot.documents.len(), 1, "{stage}");
            assert!(!root.join(JOURNAL).exists(), "{stage}");
        }
    }
    #[test]
    fn recovery_preserves_independently_edited_source_or_destination() {
        for change_target in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("bundle");
            let store = KnowledgeStore::open(&root, true).unwrap();
            let original = store.put(&document(), ExpectedRevision::Absent).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "knowledge::store::moves::tests::crash_writer",
                    "--nocapture",
                ])
                .env("YGG_MOVE_CRASH_ROOT", &root)
                .env("YGG_MOVE_CRASH_STAGE", "intent")
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(86));
            let changed = if change_target {
                Key {
                    repo: None,
                    ..original.key
                }
            } else {
                original.key
            };
            std::fs::write(root.join(changed.relative_path()), "independent edit").unwrap();
            assert!(store.find(original.key.id).is_err());
            assert_eq!(
                std::fs::read_to_string(root.join(changed.relative_path())).unwrap(),
                "independent edit"
            );
            assert!(root.join(JOURNAL).exists());
            if change_target {
                assert_eq!(
                    std::fs::read_to_string(root.join(original.key.relative_path())).unwrap(),
                    original.document.serialize().unwrap()
                );
            }
        }
    }
}
