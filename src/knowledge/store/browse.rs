//! Explicit document inspection, independent of Yggdrasil profiles or activation.
use super::*;
use std::os::unix::fs::MetadataExt;

const MAX_ENTRIES: usize = 100_000;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct BrowseDocument {
    pub path: String,
    pub revision: String,
    pub document_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct BrowseReport {
    pub documents: Vec<BrowseDocument>,
    pub diagnostics: Vec<String>,
    pub shared_commit: Option<String>,
    pub shared_current: Option<bool>,
}

fn supported_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 4096
            && path.split('/').count() <= 32
            && path
                .split('/')
                .all(|p| !p.is_empty() && !p.starts_with('.') && p.len() <= 255)
            && !path.contains('\\')
            && !path.chars().any(char::is_control),
        "browse requires a non-hidden relative bundle path"
    );
    Ok(())
}

fn inspect(
    parent: &File,
    name: &str,
    path: &str,
    text: bool,
    bytes: &mut usize,
) -> Result<BrowseDocument> {
    let file = child(parent, name, libc::O_RDONLY, 0)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "browse requires a regular non-hardlinked document"
    );
    ensure!(
        metadata.len() <= MAX_DOCUMENT_BYTES as u64,
        "document exceeds byte limit"
    );
    ensure!(
        *bytes + metadata.len() as usize <= MAX_TOTAL_BYTES,
        "browse exceeds total byte limit"
    );
    let mut contents = Vec::new();
    let read = file
        .take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut contents);
    // Invalid UTF-8 and interrupted reads consume the budget too.
    *bytes += contents.len();
    ensure!(*bytes <= MAX_TOTAL_BYTES, "browse exceeds total byte limit");
    read?;
    let contents = String::from_utf8(contents)?;
    let doc = Document::parse(&contents)?;
    Ok(BrowseDocument {
        path: path.into(),
        revision: digest(contents.as_bytes()),
        document_type: doc.document_type()?.into(),
        text: text.then_some(contents),
    })
}

fn walk(
    dir: &File,
    prefix: &str,
    entries: &mut usize,
    bytes: &mut usize,
    report: &mut BrowseReport,
) -> Result<()> {
    let mut children = names_limited(dir, MAX_ENTRIES - *entries)?;
    children.sort();
    for name in children {
        *entries += 1;
        ensure!(*entries <= MAX_ENTRIES, "browse exceeds entry limit");
        if name.starts_with('.') {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if let Err(error) = supported_path(&path) {
            report.diagnostics.push(format!("{path:?}: {error}"));
            continue;
        }
        match directory(dir, &name, false) {
            Ok(nested) => walk(&nested, &path, entries, bytes, report)?,
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                if !name.ends_with(".md") {
                    continue;
                }
                // Stop the traversal at the cumulative limit; never silently
                // omit an arbitrary suffix while claiming a complete listing.
                ensure!(*bytes < MAX_TOTAL_BYTES, "browse exceeds total byte limit");
                match inspect(dir, &name, &path, false, bytes) {
                    Ok(doc) => report.documents.push(doc),
                    Err(error) => report.diagnostics.push(format!("{path:?}: {error}")),
                }
            }
            Err(error) => report.diagnostics.push(format!("{path:?}: {error}")),
        }
    }
    Ok(())
}

impl KnowledgeStore {
    /// SQL reversal cannot represent generic or misplaced OKF documents. Compare
    /// all visible Markdown with the profiled snapshot before accepting it.
    pub(in crate::knowledge) fn require_representable_snapshot(
        &self,
        snapshot: &Snapshot,
    ) -> Result<()> {
        let report = self.browse(None)?;
        ensure!(
            report.diagnostics.is_empty(),
            "incomplete recovery browsing: {:?}",
            report.diagnostics
        );
        let profiled: std::collections::BTreeMap<_, _> = snapshot
            .documents
            .iter()
            .map(|doc| {
                (
                    doc.key.relative_path().to_string_lossy().into_owned(),
                    doc.revision.as_str(),
                )
            })
            .collect();
        ensure!(
            report.documents.len() == profiled.len()
                && report
                    .documents
                    .iter()
                    .all(|doc| profiled.get(&doc.path) == Some(&doc.revision.as_str())),
            "recovery corpus contains generic, misplaced or changed documents that SQL cannot represent"
        );
        Ok(())
    }
    /// Inspect visible Markdown throughout the bundle, without interpreting any
    /// profile as authority. A path returns exact source text; a listing retains
    /// only metadata and hashes. Diagnostics explicitly mark incomplete results.
    pub fn browse(&self, path: Option<&str>) -> Result<BrowseReport> {
        let mut report = BrowseReport::default();
        if let Some(path) = path {
            supported_path(path)?;
            ensure!(path.ends_with(".md"), "browse requires a Markdown document");
            let mut parent = self.root.try_clone()?;
            let parts = path.split('/').collect::<Vec<_>>();
            for part in &parts[..parts.len() - 1] {
                parent = directory(&parent, part, false)?;
            }
            report
                .documents
                .push(inspect(&parent, parts.last().unwrap(), path, true, &mut 0)?);
        } else if let Err(error) = walk(&self.root, "", &mut 0, &mut 0, &mut report) {
            report.diagnostics.push(error.to_string());
        }
        Ok(report)
    }
}
