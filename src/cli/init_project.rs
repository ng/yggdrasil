//! `ygg init-project` — install / update / remove Yggdrasil's managed
//! integration block in a project's `CLAUDE.md` and `AGENTS.md`.
//!
//! Idempotent via versioned HTML-comment markers. Re-running the same
//! binary version on an already-configured repo is a no-op. Running a
//! newer binary refreshes the block. User content outside the markers
//! is never touched.

use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const BEGIN: &str = "<!-- BEGIN YGG INTEGRATION";
const END: &str = "<!-- END YGG INTEGRATION -->";

/// The imperative block injected into CLAUDE.md. Keep this in sync with
/// the guidance surfaced by `ygg prime`.
const CLAUDE_BLOCK: &str = r#"## Yggdrasil Agent Coordination

This project uses **Yggdrasil** (`ygg`) for cross-session memory, resource
coordination, and issue tracking. Hooks run automatically at Claude Code
lifecycle events; you do not need to invoke them.

### Rules

- **Task tracking** — use `ygg task` for any work that outlives the current turn.
  Intra-turn checklists can stay in a native TodoList; cross-session work lives
  in `ygg task`. Do **not** use `bd` / beads.
- **Durable notes** — `ygg remember "..."` writes a repo-scoped directive the
  similarity retriever will surface in future sessions. Prefer this over
  scratch markdown files.
- **Before editing shared resources** — `ygg lock acquire <path-or-key>`. Release
  when done. Locks are advisory; bypassing them defeats the coordination layer.
- **Read `[ygg memory | ...]` injections** at the top of each user turn. They
  are real prior context surfaced by similarity — treat as relevant unless the
  content clearly refutes that.
- **For parallel work** that warrants its own context window, prefer
  `ygg spawn --task "..."` over the native Task/Agent tool.

### Quick reference

```bash
ygg task ready                  # unblocked tasks in this repo
ygg task create "title" [--kind bug|feature|... --priority 0..4]
ygg task claim <ref>            # take a task (assignee + in_progress)
ygg task close <ref> [--reason "..."]
ygg task show <ref>             # full detail for <prefix>-N or UUID
ygg task dep <task> <blocker>   # record dependency

ygg remember "..."              # durable repo-scoped note
ygg lock acquire <key>          # lease a shared resource
ygg lock release <key>          # release when done
ygg status                      # other agents + outstanding locks
ygg logs --follow               # live event stream
```

### Session completion

Work is not complete until `git push` succeeds. Release held locks, run quality
gates, rebase, push, verify `git status` shows up-to-date.
"#;

/// The block for AGENTS.md — slightly terser, same semantics.
const AGENTS_BLOCK: &str = r#"## Yggdrasil Coordination

This project uses **Yggdrasil** (`ygg`) for cross-session memory and
coordination. Hooks fire at lifecycle events; you do not invoke them manually.

### Rules

- Use `ygg task` for cross-session work (not `bd` / beads).
- Use `ygg remember "..."` for durable repo-scoped notes.
- Acquire `ygg lock` before editing a shared resource; release when done.
- Read `[ygg memory | ...]` injections — they are real prior context.
- Prefer `ygg spawn` over a native Task tool for parallel work.

### Quick reference

```bash
ygg task ready | list | create | claim | close | show | dep
ygg remember "..."
ygg lock acquire <key> | release <key> | list
ygg status | logs --follow
```

Work is not complete until `git push` succeeds.
"#;

/// Compute a stable content hash of the template at this version.
/// We only need to detect "the block needs updating"; any change to the
/// template bytes will flip this.
fn block_hash(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn begin_marker(hash: &str) -> String {
    format!("{BEGIN} v:{VERSION} hash:{hash} -->")
}

enum FoundBlock {
    Missing,
    UpToDate,
    Stale,
}

/// Returns the [begin..=end] byte range of an existing managed block,
/// or None if not present.
fn find_block(content: &str) -> Option<(usize, usize)> {
    let start = content.find(BEGIN)?;
    let end_from = start + BEGIN.len();
    let rel_end = content[end_from..].find(END)?;
    let end = end_from + rel_end + END.len();
    Some((start, end))
}

fn classify(content: &str, current_hash: &str) -> FoundBlock {
    match find_block(content) {
        None => FoundBlock::Missing,
        Some((s, e)) => {
            let block = &content[s..e];
            if block.contains(&format!("hash:{current_hash}")) {
                FoundBlock::UpToDate
            } else {
                FoundBlock::Stale
            }
        }
    }
}

/// Install or update the block in `content`. Returns the new content.
fn install_block(content: &str, body: &str) -> String {
    let hash = block_hash(body);
    let managed = format!("{}\n{}\n{}", begin_marker(&hash), body.trim_end(), END);

    if let Some((s, e)) = find_block(content) {
        // Replace existing block in place.
        let mut out = String::with_capacity(content.len() + managed.len());
        out.push_str(&content[..s]);
        out.push_str(&managed);
        out.push_str(&content[e..]);
        out
    } else if content.trim().is_empty() {
        // Empty file: block only.
        format!("{managed}\n")
    } else {
        // Append, preceded by a blank line.
        let mut out = String::with_capacity(content.len() + managed.len() + 2);
        out.push_str(content.trim_end());
        out.push_str("\n\n");
        out.push_str(&managed);
        out.push('\n');
        out
    }
}

/// Remove the managed block from `content` (if present). Returns the new content.
fn remove_block(content: &str) -> String {
    match find_block(content) {
        None => content.to_string(),
        Some((s, e)) => {
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..s]);
            // Eat a trailing newline from the block end if present
            let rest = &content[e..];
            out.push_str(rest.trim_start_matches('\n'));
            // Collapse leading blank lines the user left before the block
            out.trim_end().to_string() + "\n"
        }
    }
}

/// Public entry point. Target files are `<cwd>/CLAUDE.md` and `<cwd>/AGENTS.md`.
pub fn install(cwd: &Path) -> Result<InstallReport, anyhow::Error> {
    let mut report = InstallReport::default();
    for (filename, body) in [("CLAUDE.md", CLAUDE_BLOCK), ("AGENTS.md", AGENTS_BLOCK)] {
        let path = cwd.join(filename);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let hash = block_hash(body);
        let action = match (classify(&existing, &hash), path.exists()) {
            (FoundBlock::UpToDate, _)     => ActionTaken::Unchanged,
            (FoundBlock::Stale, _)         => ActionTaken::Updated,
            (FoundBlock::Missing, true)    => ActionTaken::Appended,
            (FoundBlock::Missing, false)   => ActionTaken::Created,
        };
        if !matches!(action, ActionTaken::Unchanged) {
            let new_content = install_block(&existing, body);
            std::fs::write(&path, new_content)?;
        }
        report.files.push((path, action));
    }
    Ok(report)
}

pub fn remove(cwd: &Path) -> Result<InstallReport, anyhow::Error> {
    let mut report = InstallReport::default();
    for filename in ["CLAUDE.md", "AGENTS.md"] {
        let path = cwd.join(filename);
        if !path.exists() {
            report.files.push((path, ActionTaken::Unchanged));
            continue;
        }
        let existing = std::fs::read_to_string(&path)?;
        let action = match find_block(&existing) {
            None => ActionTaken::Unchanged,
            Some(_) => ActionTaken::Removed,
        };
        if matches!(action, ActionTaken::Removed) {
            let new_content = remove_block(&existing);
            // If after removal only the block-less file is empty → delete file.
            if new_content.trim().is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                std::fs::write(&path, new_content)?;
            }
        }
        report.files.push((path, action));
    }
    Ok(report)
}

/// Check whether either target file already exists and has content outside
/// a managed block — used to decide whether `ygg init` should auto-invoke.
pub fn has_any_content(cwd: &Path) -> bool {
    for filename in ["CLAUDE.md", "AGENTS.md"] {
        let path = cwd.join(filename);
        if path.exists() {
            if let Ok(c) = std::fs::read_to_string(&path) {
                // Content exists if there's anything outside the managed block.
                let stripped = remove_block(&c);
                if !stripped.trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub files: Vec<(PathBuf, ActionTaken)>,
}

#[derive(Debug, Clone, Copy)]
pub enum ActionTaken {
    Created,
    Appended,
    Updated,
    Unchanged,
    Removed,
}

impl std::fmt::Display for ActionTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created   => write!(f, "created"),
            Self::Appended  => write!(f, "block appended"),
            Self::Updated   => write!(f, "block updated"),
            Self::Unchanged => write!(f, "up to date"),
            Self::Removed   => write!(f, "block removed"),
        }
    }
}

pub fn print_report(report: &InstallReport) {
    for (path, action) in &report.files {
        let display = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        println!("  {display:<12} {action}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_into_empty() {
        let body = "hello";
        let out = install_block("", body);
        assert!(out.starts_with(BEGIN));
        assert!(out.contains("hello"));
        assert!(out.trim_end().ends_with(END));
    }

    #[test]
    fn install_appends_when_file_has_content() {
        let existing = "# My Project\n\nSome notes.\n";
        let out = install_block(existing, "block");
        assert!(out.starts_with("# My Project"));
        assert!(out.contains(BEGIN));
        assert!(out.contains("block"));
    }

    #[test]
    fn install_replaces_existing_block() {
        let start = install_block("# Title\n", "old body");
        let replaced = install_block(&start, "new body");
        assert!(replaced.contains("new body"));
        assert!(!replaced.contains("old body"));
        assert!(replaced.starts_with("# Title"));
    }

    #[test]
    fn remove_strips_block() {
        let with_block = install_block("# Title\n\nUser text.\n", "managed");
        let stripped = remove_block(&with_block);
        assert!(!stripped.contains("managed"));
        assert!(stripped.contains("# Title"));
        assert!(stripped.contains("User text."));
    }

    #[test]
    fn classify_detects_same_version() {
        let body = "same";
        let h = block_hash(body);
        let content = install_block("", body);
        assert!(matches!(classify(&content, &h), FoundBlock::UpToDate));
    }

    #[test]
    fn classify_detects_stale_hash() {
        let content = install_block("", "old");
        let new_hash = block_hash("new");
        assert!(matches!(classify(&content, &new_hash), FoundBlock::Stale));
    }
}
