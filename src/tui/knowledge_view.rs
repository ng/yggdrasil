//! Knowledge tab — browse the OKF corpus that `ygg remember` and `ygg learn`
//! write under the ygg knowledge dir. Read-only and database-free: it walks
//! the Markdown bundle through `KnowledgeService::browse_documents`, the same
//! path `ygg knowledge browse` uses, so what it shows is exactly what is on
//! disk, not what any profile claims is active.
//!
//! Documents group by scope — `global` first, then the current checkout's
//! repo, then every other bound repo. `App.scope` narrows to global + the
//! current repo (`Repo`) or shows everything (`All`). Enter loads the
//! selected document's exact source text into a right-hand pane; PgUp/PgDn
//! scroll it; Esc closes it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use uuid::Uuid;

use super::app::Scope;
use crate::knowledge::runtime::Context;
use crate::knowledge::store::BrowseReport;

/// Corpus walks read every document's bytes to hash them, so poll slower
/// than the 2 s view cadence. Scope flips and explicit actions bypass this.
const REFRESH_EVERY: Duration = Duration::from_secs(10);

/// Outcome of one off-thread corpus walk. Labels and the current repo do
/// not depend on scope, so a walk started under one scope applies cleanly
/// after the user toggles to the other.
type Walk = Result<Option<(BrowseReport, BTreeMap<Uuid, String>, Option<Uuid>)>, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRow {
    /// Bundle-relative path, as `ygg knowledge browse` prints it.
    pub path: String,
    pub document_type: String,
    pub revision: String,
    /// Portable repo the path sits under; `None` for `global/` and for
    /// generic documents outside the Yggdrasil layout.
    pub repo: Option<Uuid>,
    pub global: bool,
}

impl DocRow {
    /// `notes` / `learnings` directory under the scope, rendered as a kind.
    fn kind(&self) -> &'static str {
        let mut parts = self.path.split('/');
        let dir = if self.global {
            parts.nth(1)
        } else {
            parts.nth(2)
        };
        match dir {
            Some("notes") => "note",
            Some("learnings") => "rule",
            _ => "-",
        }
    }

    fn file_stem(&self) -> &str {
        self.path
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
            .trim_end_matches(".md")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeRow {
    Header { label: String, count: usize },
    Doc(DocRow),
}

pub struct KnowledgeView {
    pub rows: Vec<KnowledgeRow>,
    /// Index into `rows`; always points at a `Doc` when any exist.
    pub selected: usize,
    /// Viewport offset for the list, so the selection stays on screen.
    pub state: TableState,
    /// `(path, source text)` of the opened document, if any.
    pub body: Option<(String, String)>,
    pub body_scroll: u16,
    /// Last error or corpus summary, shown in the title / empty state.
    pub status: String,
    pub diagnostics: Vec<String>,
    pub shared_commit: Option<String>,
    pub shared_current: Option<bool>,
    pub loaded: bool,
    all_docs: Vec<DocRow>,
    labels: BTreeMap<Uuid, String>,
    current_repo: Option<Uuid>,
    scope: Scope,
    last_refresh: Option<Instant>,
    /// In-flight corpus walk. Walks hash every document and may touch git,
    /// so they run off the input thread; `poll` collects the result.
    walk: Option<Receiver<Walk>>,
    /// In-flight document load: the requested path and its result.
    opening: Option<(String, Receiver<Result<String, String>>)>,
}

impl Default for KnowledgeView {
    fn default() -> Self {
        Self::new()
    }
}

impl KnowledgeView {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            state: TableState::default(),
            body: None,
            body_scroll: 0,
            status: String::new(),
            diagnostics: Vec::new(),
            shared_commit: None,
            shared_current: None,
            loaded: false,
            all_docs: Vec::new(),
            labels: BTreeMap::new(),
            current_repo: None,
            scope: Scope::Repo,
            last_refresh: None,
            walk: None,
            opening: None,
        }
    }

    /// Re-filter immediately when the scope changed, and start a background
    /// corpus walk when the poll interval elapsed or `force` is set. Errors
    /// land in `status`; the previous rows stay so a transient failure does
    /// not blank the pane.
    pub fn refresh(&mut self, scope: Scope, force: bool) {
        self.poll();
        if scope != self.scope {
            self.rebuild(scope);
        }
        let due = self
            .last_refresh
            .map(|t| t.elapsed() >= REFRESH_EVERY)
            .unwrap_or(true);
        if !(force || due) || self.walk.is_some() {
            return;
        }
        self.last_refresh = Some(Instant::now());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(walk_corpus());
        });
        self.walk = Some(rx);
    }

    /// Collect finished background work without blocking. The run loop
    /// calls this every iteration while the tab is visible.
    pub fn poll(&mut self) {
        match self.walk.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(walk)) => {
                self.walk = None;
                self.finish_walk(walk);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.walk = None;
                self.loaded = true;
                self.status = "knowledge walk aborted".into();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        match self.opening.as_ref().map(|(_, rx)| rx.try_recv()) {
            Some(Ok(result)) => {
                let Some((path, _)) = self.opening.take() else {
                    return;
                };
                match result {
                    Ok(text) => {
                        self.body = Some((path, text));
                        self.body_scroll = 0;
                    }
                    Err(error) => self.status = format!("open failed: {error}"),
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.opening = None;
                self.status = "open failed: loader aborted".into();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// True while a corpus walk or document load is still running.
    pub fn is_loading(&self) -> bool {
        self.walk.is_some() || self.opening.is_some()
    }

    fn finish_walk(&mut self, walk: Walk) {
        self.loaded = true;
        match walk {
            Err(error) => self.status = error,
            Ok(None) => {
                self.status = "no selected OKF corpus — run `ygg knowledge init`".into();
                self.all_docs.clear();
                self.rebuild(self.scope);
            }
            Ok(Some((report, labels, current))) => self.apply(report, labels, current, self.scope),
        }
    }

    /// Replace the document set from a browse report. Public so tests can
    /// feed a synthetic report without a corpus on disk.
    pub fn apply(
        &mut self,
        report: BrowseReport,
        labels: BTreeMap<Uuid, String>,
        current_repo: Option<Uuid>,
        scope: Scope,
    ) {
        self.loaded = true;
        self.labels = labels;
        self.current_repo = current_repo;
        self.diagnostics = report.diagnostics;
        self.shared_commit = report.shared_commit;
        self.shared_current = report.shared_current;
        self.all_docs = report
            .documents
            .into_iter()
            .map(|doc| {
                let (global, repo) = parse_scope(&doc.path);
                DocRow {
                    path: doc.path,
                    document_type: doc.document_type,
                    revision: doc.revision,
                    repo,
                    global,
                }
            })
            .collect();
        self.status = if self.diagnostics.is_empty() {
            String::new()
        } else {
            format!(
                "{} diagnostics (listing incomplete)",
                self.diagnostics.len()
            )
        };
        self.rebuild(scope);
    }

    /// Group + filter `all_docs` into `rows` for the given scope, keeping the
    /// selection on the same path where possible.
    fn rebuild(&mut self, scope: Scope) {
        self.scope = scope;
        let keep = self.selected_doc().map(|d| d.path.clone());

        let mut docs: Vec<&DocRow> = self
            .all_docs
            .iter()
            .filter(|d| match (scope, self.current_repo) {
                (Scope::All, _) | (Scope::Repo, None) => true,
                (Scope::Repo, Some(current)) => d.global || d.repo == Some(current),
            })
            .collect();
        docs.sort_by_cached_key(|d| (self.group_rank(d), self.group_label(d), d.path.clone()));

        let mut rows = Vec::with_capacity(docs.len() + 4);
        let mut i = 0;
        while i < docs.len() {
            let label = self.group_label(docs[i]);
            let start = i;
            while i < docs.len() && self.group_label(docs[i]) == label {
                i += 1;
            }
            rows.push(KnowledgeRow::Header {
                label,
                count: i - start,
            });
            rows.extend(
                docs[start..i]
                    .iter()
                    .map(|d| KnowledgeRow::Doc((*d).clone())),
            );
        }
        self.rows = rows;

        self.selected = keep
            .and_then(|path| {
                self.rows
                    .iter()
                    .position(|r| matches!(r, KnowledgeRow::Doc(d) if d.path == path))
            })
            .unwrap_or(0);
        self.snap_to_doc(true);
        if let Some((path, _)) = &self.body
            && self.all_docs.iter().all(|d| &d.path != path)
        {
            self.body = None;
        }
    }

    /// global, current repo, labelled repos, unlabelled repos, generic docs.
    fn group_rank(&self, d: &DocRow) -> u8 {
        match d.repo {
            _ if d.global => 0,
            Some(repo) if Some(repo) == self.current_repo => 1,
            Some(repo) if self.labels.contains_key(&repo) => 2,
            Some(_) => 3,
            None => 4,
        }
    }

    fn group_label(&self, d: &DocRow) -> String {
        if d.global {
            return "global".into();
        }
        match d.repo {
            Some(repo) => self
                .labels
                .get(&repo)
                .cloned()
                .unwrap_or_else(|| format!("repo {}…", &repo.to_string()[..8])),
            None => "other".into(),
        }
    }

    pub fn selected_doc(&self) -> Option<&DocRow> {
        match self.rows.get(self.selected) {
            Some(KnowledgeRow::Doc(d)) => Some(d),
            _ => None,
        }
    }

    pub fn doc_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r, KnowledgeRow::Doc(_)))
            .count()
    }

    /// Move `selected` off a header onto the nearest doc in `forward`
    /// direction, falling back to the other direction at the edges.
    fn snap_to_doc(&mut self, forward: bool) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        let is_doc = |i: usize, rows: &[KnowledgeRow]| matches!(rows[i], KnowledgeRow::Doc(_));
        if is_doc(self.selected, &self.rows) {
            return;
        }
        let range: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new((self.selected..self.rows.len()).chain((0..self.selected).rev()))
        } else {
            Box::new(
                (0..self.selected)
                    .rev()
                    .chain(self.selected..self.rows.len()),
            )
        };
        for i in range {
            if is_doc(i, &self.rows) {
                self.selected = i;
                return;
            }
        }
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let from = self.selected;
        if let Some(next) =
            (from + 1..self.rows.len()).find(|&i| matches!(self.rows[i], KnowledgeRow::Doc(_)))
        {
            self.selected = next;
        }
    }

    pub fn select_prev(&mut self) {
        if let Some(prev) = (0..self.selected)
            .rev()
            .find(|&i| matches!(self.rows[i], KnowledgeRow::Doc(_)))
        {
            self.selected = prev;
        }
    }

    pub fn body_open(&self) -> bool {
        self.body.is_some()
    }

    pub fn close_body(&mut self) {
        self.body = None;
        self.body_scroll = 0;
        self.opening = None;
    }

    pub fn scroll_body_down(&mut self, lines: u16) {
        if self.body.is_some() {
            self.body_scroll = self.body_scroll.saturating_add(lines);
        }
    }

    pub fn scroll_body_up(&mut self, lines: u16) {
        self.body_scroll = self.body_scroll.saturating_sub(lines);
    }

    /// Enter: open the selected document, or close it if already open.
    pub fn toggle_body(&mut self) {
        let Some(path) = self.selected_doc().map(|d| d.path.clone()) else {
            return;
        };
        if self.body.as_ref().is_some_and(|(open, _)| *open == path) {
            self.close_body();
            return;
        }
        let (tx, rx) = mpsc::channel();
        let request = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(load_text(&request).map_err(|error| error.to_string()));
        });
        self.opening = Some((path, rx));
    }

    /// Install a body directly. Test seam mirroring `toggle_body` without a
    /// corpus on disk.
    pub fn set_body(&mut self, path: impl Into<String>, text: impl Into<String>) {
        self.body = Some((path.into(), text.into()));
        self.body_scroll = 0;
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = self.title();
        if self.rows.is_empty() {
            let msg = if !self.loaded {
                "  · loading…".to_string()
            } else if self.status.is_empty() {
                format!(
                    "  · no documents in scope ({})  —  `ygg remember \"...\"` writes one",
                    self.scope.label()
                )
            } else {
                format!("  · {}", self.status)
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))),
            ])
            .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let (list_area, body_area) = match &self.body {
            Some(_) => {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                    .split(area);
                (chunks[0], Some(chunks[1]))
            }
            None => (area, None),
        };

        // With the source pane open the list is half-width; drop the
        // revision column so group labels and types keep their room.
        let compact = body_area.is_some();
        let mut header_cells = vec![
            Cell::from("KIND").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from("TYPE"),
            Cell::from("ID"),
        ];
        if !compact {
            header_cells.push(Cell::from("REV"));
        }
        let header = Row::new(header_cells).height(1);

        let rows: Vec<Row> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                KnowledgeRow::Header { label, count } => Row::new(vec![Cell::from(Span::styled(
                    format!("▾ {label} ({count})"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))]),
                KnowledgeRow::Doc(d) => {
                    let style = if i == self.selected {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    };
                    let open = self.body.as_ref().is_some_and(|(p, _)| *p == d.path);
                    let kind_color = match d.kind() {
                        "note" => Color::Green,
                        "rule" => Color::Magenta,
                        _ => Color::Gray,
                    };
                    let mut cells = vec![
                        Cell::from(format!("  {}{}", if open { "▸ " } else { "" }, d.kind()))
                            .style(Style::default().fg(kind_color)),
                        Cell::from(d.document_type.clone()),
                        Cell::from(short(d.file_stem(), 8)),
                    ];
                    if !compact {
                        cells.push(
                            Cell::from(short(&d.revision, 8))
                                .style(Style::default().fg(Color::DarkGray)),
                        );
                    }
                    Row::new(cells).style(style)
                }
            })
            .collect();

        // Group headers live in the first column; widen it to fit the
        // longest label so repo names are never clipped.
        let kind_width = self
            .rows
            .iter()
            .filter_map(|r| match r {
                KnowledgeRow::Header { label, count } => {
                    Some(format!("▾ {label} ({count})").chars().count() as u16 + 1)
                }
                KnowledgeRow::Doc(_) => None,
            })
            .max()
            .unwrap_or(0)
            .max(14);
        let mut widths = vec![
            Constraint::Length(kind_width),
            Constraint::Min(if compact { 8 } else { 16 }),
            Constraint::Length(10),
        ];
        if !compact {
            widths.push(Constraint::Length(10));
        }
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(title));
        self.state.select(Some(self.selected));
        frame.render_stateful_widget(table, list_area, &mut self.state);

        if let (Some(body_area), Some((path, text))) = (body_area, &self.body) {
            let para = Paragraph::new(text.as_str())
                .wrap(Wrap { trim: false })
                .scroll((self.body_scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {path}  ·  PgUp/PgDn=scroll  Esc=close ")),
                );
            frame.render_widget(para, body_area);
        }
    }

    fn title(&self) -> String {
        let mut parts = vec![format!("Knowledge ({} docs)", self.doc_count())];
        parts.push(format!("scope: {}", self.scope.label()));
        if let Some(commit) = &self.shared_commit {
            let state = match self.shared_current {
                Some(true) => "current",
                Some(false) => "stale",
                None => "unknown",
            };
            parts.push(format!("shared {} ({state})", short(commit, 8)));
        }
        if !self.status.is_empty() {
            parts.push(self.status.clone());
        }
        parts.push("Enter=open".into());
        format!(" {} ", parts.join(" · "))
    }
}

/// Split a bundle path into (is_global, repo). Layout is fixed by
/// `store::Key::relative_path`: `global/<kind>/<id>.md` or
/// `repos/<uuid>/<kind>/<id>.md`. Anything else is a generic document.
fn parse_scope(path: &str) -> (bool, Option<Uuid>) {
    let mut parts = path.split('/');
    match parts.next() {
        Some("global") => (true, None),
        Some("repos") => (false, parts.next().and_then(|id| id.parse().ok())),
        _ => (false, None),
    }
}

fn walk_corpus() -> Walk {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let context = match Context::for_browse(crate::config::database::process_environment()) {
        Err(error) => return Err(format!("knowledge unavailable: {error}")),
        Ok(None) => return Ok(None),
        Ok(Some(context)) => context,
    };
    let labels = context.repo_labels().unwrap_or_default();
    let current = context.resolve_repo(&cwd).ok().flatten();
    context
        .service
        .browse_documents(None)
        .map(|report| Some((report, labels, current)))
        .map_err(|error| format!("knowledge browse failed: {error}"))
}

fn load_text(path: &str) -> anyhow::Result<String> {
    let context = Context::for_browse(crate::config::database::process_environment())?
        .ok_or_else(|| anyhow::anyhow!("no selected OKF corpus"))?;
    let report = context.service.browse_documents(Some(path))?;
    report
        .documents
        .into_iter()
        .next()
        .and_then(|d| d.text)
        .ok_or_else(|| anyhow::anyhow!("document returned no text"))
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
