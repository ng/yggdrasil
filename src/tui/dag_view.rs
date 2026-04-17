//! Task dependency graph — rolls tasks up into epic → task → deps.
//! This is the "what's the shape of the work?" view, matching how
//! beads renders issues. Different from the flat list in the Tasks
//! pane: here we see roots (epics, unblocked tasks) at the top and
//! their children indented below, with dependency arrows visible.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

use crate::models::repo::{Repo, RepoRepo};
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DagSort { Priority, Kind, Recent }

impl DagSort {
    fn label(&self) -> &'static str {
        match self { Self::Priority => "priority", Self::Kind => "kind", Self::Recent => "recent" }
    }
    fn next(&self) -> Self {
        match self { Self::Priority => Self::Kind, Self::Kind => Self::Recent, Self::Recent => Self::Priority }
    }
}

pub struct DagView {
    pub rows: Vec<RenderRow>,
    pub state: ListState,
    pub repo_name: String,
    /// What happened on the last refresh. Shown in the empty-state so it
    /// is obvious whether we have an error, a registered repo with no
    /// tasks, or a fallback with no conversation either.
    pub last_status: String,
    pub detail_open: bool,
    pub sort: DagSort,
}

pub enum RenderRow {
    RepoHeader { prefix: String, name: String, open_count: usize },
    Task {
        task: Task,
        prefix: String,
        depth: usize,
        is_root: bool,
        n_children: usize,
    },
}

impl DagView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self { rows: vec![], state: st, repo_name: String::new(),
            last_status: String::new(), detail_open: false, sort: DagSort::Priority }
    }

    // Kept for compatibility with existing app.rs entry — a no-op here
    // since we're scoped to the current-cwd repo.
    pub fn set_agent(&mut self, _name: String) {}

    pub fn scroll_up(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(if i == 0 { self.rows.len() - 1 } else { i - 1 }));
    }

    pub fn scroll_down(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.rows.len()));
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
    }

    /// Toggle the detail overlay for the selected task row. Does nothing for
    /// repo-header rows since there's no task to show.
    pub fn toggle_detail(&mut self) {
        if let Some(i) = self.state.selected() {
            if matches!(self.rows.get(i), Some(RenderRow::Task { .. })) {
                self.detail_open = !self.detail_open;
            }
        }
    }

    fn selected_task(&self) -> Option<(&Task, &str)> {
        let i = self.state.selected()?;
        match self.rows.get(i)? {
            RenderRow::Task { task, prefix, .. } => Some((task, prefix.as_str())),
            _ => None,
        }
    }

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        // Whole-system view: show all open tasks across every repo, grouped
        // by repo header. Deliberately NOT cwd-scoped — the TUI is a
        // cross-project dashboard; the CLI `ygg task ready` is the
        // in-repo quick view.
        self.repo_name.clear();
        self.last_status.clear();

        let repos = RepoRepo::new(pool).list().await.unwrap_or_default();

        // All open tasks across every repo, keyed by repo_id.
        let all_open: Vec<Task> = TaskRepo::new(pool).list(None, None).await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .collect();

        if all_open.is_empty() {
            self.rows.clear();
            self.last_status = format!(
                "no open tasks in any of {} registered repo(s)", repos.len()
            );
            self.state.select(None);
            return Ok(());
        }

        // Bucket by repo.
        let mut by_repo: HashMap<Uuid, Vec<Task>> = HashMap::new();
        for t in all_open {
            by_repo.entry(t.repo_id).or_default().push(t);
        }

        // Preload all dep edges for all open tasks in one query.
        let every_id: Vec<Uuid> = by_repo.values().flat_map(|v| v.iter().map(|t| t.task_id)).collect();
        let edges: Vec<(Uuid, Uuid)> = sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT task_id, blocker_id FROM task_deps
             WHERE task_id = ANY($1) OR blocker_id = ANY($1)"
        )
        .bind(&every_id)
        .fetch_all(pool).await.unwrap_or_default();
        let mut children_of: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        let mut blockers_of: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for (t, b) in &edges {
            children_of.entry(*b).or_default().push(*t);
            blockers_of.entry(*t).or_default().push(*b);
        }

        // Stable repo order: by name asc.
        let mut repo_order: Vec<&Repo> = repos.iter()
            .filter(|r| by_repo.contains_key(&r.repo_id))
            .collect();
        repo_order.sort_by(|a, b| a.name.cmp(&b.name));

        let mut rows: Vec<RenderRow> = Vec::new();
        for repo in repo_order {
            let tasks = by_repo.remove(&repo.repo_id).unwrap_or_default();
            rows.push(RenderRow::RepoHeader {
                prefix: repo.task_prefix.clone(),
                name: repo.name.clone(),
                open_count: tasks.len(),
            });

            let by_id: BTreeMap<Uuid, &Task> = tasks.iter().map(|t| (t.task_id, t)).collect();

            let mut roots: Vec<&Task> = tasks.iter()
                .filter(|t| {
                    let no_blockers = blockers_of.get(&t.task_id)
                        .map(|bs| bs.iter().all(|b| !by_id.contains_key(b)))
                        .unwrap_or(true);
                    matches!(t.kind, TaskKind::Epic) || no_blockers
                })
                .collect();
            sort_tasks(&mut roots, self.sort);

            let mut visited: HashSet<Uuid> = HashSet::new();
            for r in &roots {
                walk(
                    r.task_id, 0, &repo.task_prefix, &children_of, &by_id,
                    &mut visited, &mut rows, self.sort,
                );
            }
            // Cycle orphans and anything we missed.
            for t in &tasks {
                if !visited.contains(&t.task_id) {
                    let n_children = children_of.get(&t.task_id).map(|v| v.len()).unwrap_or(0);
                    rows.push(RenderRow::Task {
                        task: t.clone(),
                        prefix: repo.task_prefix.clone(),
                        depth: 0,
                        is_root: true,
                        n_children,
                    });
                    visited.insert(t.task_id);
                }
            }
        }

        self.rows = rows;

        if self.rows.is_empty() {
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= self.rows.len() {
            self.state.select(Some(0));
        }
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(" Task graph — {} open across all repos  ·  sort: {}  (s to cycle) ",
            self.rows.iter().filter(|r| matches!(r, RenderRow::Task { .. })).count(),
            self.sort.label());

        if self.rows.is_empty() {
            let lines: Vec<Line> = vec![
                Line::from(""),
                Line::from("  No open tasks in any registered repo yet."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Try "),
                    Span::styled("ygg task create \"...\" --kind task --priority 2",
                        Style::default().fg(Color::Cyan)),
                    Span::raw(" from inside a project."),
                ]),
                Line::from(""),
                if !self.last_status.is_empty() {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(Color::DarkGray)),
                        Span::styled(self.last_status.clone(), Style::default().fg(Color::DarkGray)),
                    ])
                } else { Line::from("") },
            ];
            let para = ratatui::widgets::Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let items: Vec<ListItem> = self.rows.iter().map(|r| match r {
            RenderRow::RepoHeader { prefix, name, open_count } => {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  ▸ {name}"),
                        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  ({prefix}, {open_count} open)"),
                        Style::default().fg(Color::DarkGray)),
                ]))
            }
            RenderRow::Task { task: t, prefix, depth, is_root: _, n_children } => {
                let indent = "  ".repeat(depth + 1);
                let connector = if *depth == 0 { "" } else { "└─ " };

                let (kind_color, kind_glyph) = match t.kind {
                    TaskKind::Epic    => (Color::Magenta, "◉"),
                    TaskKind::Feature => (Color::Cyan,    "✚"),
                    TaskKind::Bug     => (Color::Red,     "✗"),
                    TaskKind::Chore   => (Color::DarkGray,"·"),
                    TaskKind::Task    => (Color::White,   "○"),
                };

                let (status_color, status_label) = match t.status {
                    TaskStatus::Open        => (Color::Gray,    "open"),
                    TaskStatus::InProgress  => (Color::Yellow,  "wip "),
                    TaskStatus::Blocked     => (Color::Red,     "blkd"),
                    TaskStatus::Closed      => (Color::DarkGray,"done"),
                };

                let prio_style = match t.priority {
                    0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    1 => Style::default().fg(Color::Yellow),
                    2 => Style::default().fg(Color::White),
                    _ => Style::default().fg(Color::DarkGray),
                };

                let children_badge = if *n_children > 0 {
                    format!(" {}↓", n_children)
                } else { String::new() };

                let id = format!("{}-{}", prefix, t.seq);

                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::raw(connector),
                    Span::styled(format!("{kind_glyph} "), Style::default().fg(kind_color)),
                    Span::styled(status_label, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(format!("P{}", t.priority), prio_style),
                    Span::raw(" "),
                    Span::styled(id, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::raw(truncate(&t.title, 70)),
                    Span::styled(children_badge, Style::default().fg(Color::Cyan)),
                ]))
            }
        }).collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(list, area, &mut self.state);

        // Detail overlay floats over the list when toggled on.
        if self.detail_open {
            if let Some((task, prefix)) = self.selected_task() {
                render_detail_overlay(frame, area, task, prefix);
            }
        }
    }
}

fn render_detail_overlay(frame: &mut Frame, area: Rect, task: &Task, prefix: &str) {
    // Center a popup inside the pane area.
    let popup_w = area.width.saturating_sub(8).min(90);
    let popup_h = area.height.saturating_sub(4).min(24);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect { x, y, width: popup_w, height: popup_h };

    frame.render_widget(ratatui::widgets::Clear, popup);

    let id = format!("{}-{}", prefix, task.seq);
    let kind = format!("{:?}", task.kind).to_lowercase();
    let status = format!("{:?}", task.status).to_lowercase();

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(format!(" {id} "),
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(&task.title, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(format!("  {kind} · P{} · {status}", task.priority),
                Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
    ];

    if !task.description.is_empty() {
        lines.push(Line::from(Span::styled("description",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
        for l in task.description.lines() { lines.push(Line::from(format!("  {l}"))); }
        lines.push(Line::from(""));
    }
    if let Some(a) = task.acceptance.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled("acceptance",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
        for l in a.lines() { lines.push(Line::from(format!("  {l}"))); }
        lines.push(Line::from(""));
    }
    if let Some(d) = task.design.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled("design",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
        for l in d.lines() { lines.push(Line::from(format!("  {l}"))); }
        lines.push(Line::from(""));
    }
    if let Some(n) = task.notes.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled("notes",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))));
        for l in n.lines() { lines.push(Line::from(format!("  {l}"))); }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" task detail — Enter/Esc to close ")
        .border_style(Style::default().fg(Color::Cyan));
    let para = ratatui::widgets::Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(para, popup);
}

fn walk(
    id: Uuid,
    depth: usize,
    prefix: &str,
    children_of: &HashMap<Uuid, Vec<Uuid>>,
    by_id: &BTreeMap<Uuid, &Task>,
    visited: &mut HashSet<Uuid>,
    rows: &mut Vec<RenderRow>,
    sort: DagSort,
) {
    if !visited.insert(id) { return; }
    let Some(task) = by_id.get(&id).copied() else { return; };
    let children = children_of.get(&id).cloned().unwrap_or_default();
    let n_children = children.iter().filter(|c| by_id.contains_key(c)).count();

    rows.push(RenderRow::Task {
        task: task.clone(),
        prefix: prefix.to_string(),
        depth,
        is_root: depth == 0,
        n_children,
    });

    let mut sorted: Vec<&Task> = children.iter()
        .filter_map(|c| by_id.get(c).copied()).collect();
    sort_tasks(&mut sorted, sort);

    for child in sorted {
        walk(child.task_id, depth + 1, prefix, children_of, by_id, visited, rows, sort);
    }
}

fn sort_tasks(tasks: &mut Vec<&Task>, sort: DagSort) {
    match sort {
        DagSort::Priority => tasks.sort_by_key(|t| (t.priority, t.seq)),
        DagSort::Kind => tasks.sort_by_key(|t| (kind_order(&t.kind), t.priority, t.seq)),
        DagSort::Recent => tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at)),
    }
}

fn kind_order(k: &TaskKind) -> u8 {
    match k {
        TaskKind::Epic => 0,
        TaskKind::Feature => 1,
        TaskKind::Bug => 2,
        TaskKind::Task => 3,
        TaskKind::Chore => 4,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

