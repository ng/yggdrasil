//! Disposable metadata lookup. Fingerprints invalidate rows; selected bytes
//! remain authoritative and are always parsed again before injection.
use super::*;
use crate::knowledge::document::{PARSER_VERSION, Profile};
use std::{collections::BTreeMap, os::unix::fs::MetadataExt};

fn index_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Note => ".lookup-notes.json",
        Kind::Learning => ".lookup-rules.json",
    }
}
const LIMIT: usize = 64 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn at(parent: &File, name: &str) -> Result<Self> {
        let name = CString::new(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstatat initializes the supplied stat on success. Do not follow
        // final symlinks, even for advisory cache fingerprint checks.
        let status = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stat = unsafe { stat.assume_init() };
        ensure!(
            stat.st_mode & libc::S_IFMT == libc::S_IFREG,
            "knowledge document is not a regular file"
        );
        Ok(Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            size: stat.st_size as u64,
            mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            ctime: (stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        })
    }
    fn from_file(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        ensure!(m.is_file(), "knowledge document is not a regular file");
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub key: Key,
    pub revision: String,
    pub profile: Profile,
    stamp: Stamp,
}

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
    parser: u32,
    kind: Kind,
    /// Digest of the complete sorted path/fingerprint/document-digest rows for this kind.
    corpus_revision: String,
}

#[derive(Default)]
pub(crate) struct Candidates {
    pub rows: Vec<Candidate>,
    pub diagnostics: Vec<String>,
}

impl KnowledgeStore {
    fn read_index(&self, kind: Kind) -> Result<BTreeMap<String, Candidate>> {
        let text = Self::read_limited(&self.root, index_name(kind), LIMIT)?
            .ok_or_else(|| anyhow::anyhow!("lookup index absent"))?;
        let (header, body) = text
            .split_once('\n')
            .ok_or_else(|| anyhow::anyhow!("invalid lookup header"))?;
        ensure!(header.len() <= 1024, "lookup header exceeds limit");
        let header: Header = serde_json::from_str(header)?;
        ensure!(
            header.version == 2 && header.parser == PARSER_VERSION && header.kind == kind,
            "lookup version changed"
        );
        ensure!(
            header.corpus_revision == digest(body.as_bytes()),
            "lookup checksum mismatch"
        );
        Ok(serde_json::from_str(body)?)
    }

    pub(crate) fn candidates(&self, kind: Kind) -> Candidates {
        let mut result = Candidates::default();
        let phase = crate::knowledge::timing::Phase::start("index_recovery");
        if let Err(e) = self.recover_move() {
            result.diagnostics.push(format!("scope move recovery: {e}"));
            return result;
        }
        drop(phase);
        let phase = crate::knowledge::timing::Phase::start("index_inventory");
        let inventory = self.inventory_for(Some(kind));
        drop(phase);
        result.diagnostics.extend(inventory.diagnostics);
        if inventory.incomplete {
            return result;
        }
        // Missing, damaged, stale-version or unwritable caches are rebuildable.
        // Their failure cannot erase authoritative documents or fail a read.
        let phase = crate::knowledge::timing::Phase::start("index_read");
        let loaded = self.read_index(kind);
        drop(phase);
        let phase = crate::knowledge::timing::Phase::start("index_validate");
        let mut dirty = loaded.is_err();
        let mut old = loaded.unwrap_or_default();
        let old_len = old.len();
        let mut counts = std::collections::HashMap::new();
        for key in &inventory.keys {
            *counts.entry(key.id).or_insert(0) += 1;
        }
        // Inventory groups rows by scope/kind. Reuse only the current anchored
        // directory descriptor, bounding open descriptors independently of size.
        let mut parent: Option<(Option<Uuid>, Kind, File)> = None;
        // Inventory and fingerprint the requested kind. A candidate's UUID is
        // checked across every kind/scope immediately before loading its bytes.
        // Other kinds produce diagnostics in their own lookups or full browsing.
        for key in inventory.keys {
            if counts[&key.id] != 1 {
                let path = key.relative_path().to_string_lossy().into_owned();
                result
                    .diagnostics
                    .push(format!("{path}: duplicate document UUID"));
                continue;
            }
            if key.kind != kind {
                continue;
            }
            let path = key.relative_path().to_string_lossy().into_owned();
            let row = (|| -> Result<Candidate> {
                if !parent
                    .as_ref()
                    .is_some_and(|(repo, kind, _)| *repo == key.repo && *kind == key.kind)
                {
                    parent = Some((key.repo, key.kind, self.parent(key, false)?));
                }
                let directory = &parent.as_ref().unwrap().2;
                let name = format!("{}.md", key.id);
                let before = Stamp::at(directory, &name)?;
                if let Some(row) = old
                    .remove(&path)
                    .filter(|r| r.key == key && r.stamp == before)
                {
                    return Ok(row);
                }
                dirty = true;
                let mut file = child(directory, &name, libc::O_RDONLY, 0)?;
                ensure!(
                    before == Stamp::from_file(&file)?,
                    "document replaced while indexing"
                );
                let mut text = String::new();
                (&mut file)
                    .take((MAX_DOCUMENT_BYTES + 1) as u64)
                    .read_to_string(&mut text)?;
                ensure!(
                    before == Stamp::from_file(&file)?,
                    "document changed while indexing"
                );
                let document = Document::parse(&text)?;
                ensure!(
                    Key::from_document(&document)? == key,
                    "document identity does not match bundle path"
                );
                let mut profile = document.profile()?.unwrap();
                // Only matching/sort fields belong in this advisory index. Never
                // retain activation evidence or unknown metadata as authority.
                profile.approval = None;
                profile.extra.clear();
                Ok(Candidate {
                    key,
                    revision: digest(text.as_bytes()),
                    profile,
                    stamp: before,
                })
            })();
            match row {
                Ok(row) => result.rows.push(row),
                Err(e) => result.diagnostics.push(format!("{path}: {e}")),
            }
        }
        drop(phase);
        dirty |= old_len != result.rows.len();
        if dirty {
            let save = (|| -> Result<()> {
                // Warm reads move validated rows into the result without copying
                // profiles or constructing a second index. Only changed indexes
                // need a sorted serialization view; borrowed rows preserve format.
                let next: BTreeMap<_, _> = result
                    .rows
                    .iter()
                    .map(|row| (row.key.relative_path().to_string_lossy().into_owned(), row))
                    .collect();
                let body = serde_json::to_string(&next)?;
                let header = serde_json::to_string(&Header {
                    version: 2,
                    parser: PARSER_VERSION,
                    kind,
                    corpus_revision: digest(body.as_bytes()),
                })?;
                let text = format!("{header}\n{body}");
                ensure!(text.len() <= LIMIT, "lookup index exceeds byte limit");
                // Cache writers may publish an older observation concurrently;
                // the next read checks every live fingerprint again regardless.
                Self::replace_at(&self.root, index_name(kind), &text)
            })();
            if let Err(e) = save {
                tracing::debug!(error = %e, "disposable knowledge index was not saved");
            }
        }
        result
    }

    pub(crate) fn load_candidates(&self, rows: &[Candidate]) -> super::Snapshot {
        let selected: Vec<_> = rows
            .iter()
            .map(|row| (row.key, row.revision.as_str()))
            .collect();
        self.revalidate_keys(&selected)
    }

    pub(crate) fn load_candidate(&self, row: &Candidate) -> Result<Option<RevisionedDocument>> {
        let inventory = self.inventory_ids([row.key.id]);
        ensure!(
            !inventory.incomplete,
            "cannot resolve UUID in an incomplete bundle inventory: {:?}",
            inventory.diagnostics
        );
        ensure!(inventory.keys.len() <= 1, "duplicate document UUID");
        if inventory.keys.is_empty() {
            return Ok(None);
        }
        Ok(self
            .get_unchecked(row.key)?
            .filter(|current| current.revision == row.revision))
    }
}
