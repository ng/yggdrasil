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

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};

/// If no tasks in this repo, we fall back to recent conversation nodes
/// for the agent that matches the current cwd — otherwise the DAG pane
/// would be useless in every repo that hasn't created a task yet.
pub enum FallbackRow {
    Convo {
        kind: String,
        snippet: String,
        age_secs: i64,
    },
}

pub struct DagView {
    pub rows: Vec<RenderRow>,
    pub fallback: Vec<FallbackRow>,
    pub state: ListState,
    pub repo_name: String,
}

pub struct RenderRow {
    pub task: Task,
    pub depth: usize,
    pub is_root: bool,
    pub n_children: usize,
}

impl DagView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self { rows: vec![], fallback: vec![], state: st, repo_name: String::new() }
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

    pub async fn refresh(&mut self, pool: &PgPool) -> Result<(), anyhow::Error> {
        let repo = match resolve_cwd_repo(pool).await {
            Ok(r) => r,
            Err(_) => { self.rows.clear(); self.repo_name = "?".into(); return Ok(()); }
        };
        self.repo_name = format!("{} ({})", repo.name, repo.task_prefix);

        // All non-closed tasks in this repo.
        let all = TaskRepo::new(pool).list(Some(repo.repo_id), None).await.unwrap_or_default();
        let open: Vec<Task> = all.into_iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .collect();

        // Pull dependency edges in one query: for each task, which tasks does
        // IT depend on (blockers). Build parent→children as "this task is a
        // child OF its blockers" so roots are tasks with no blockers.
        let task_ids: Vec<Uuid> = open.iter().map(|t| t.task_id).collect();
        let edges: Vec<(Uuid, Uuid)> = if task_ids.is_empty() {
            vec![]
        } else {
            sqlx::query_as::<_, (Uuid, Uuid)>(
                "SELECT task_id, blocker_id FROM task_deps
                 WHERE task_id = ANY($1) OR blocker_id = ANY($1)"
            )
            .bind(&task_ids)
            .fetch_all(pool)
            .await
            .unwrap_or_default()
        };

        // blocker_id → [task_id, ...]  (the blocker's "children" in tree terms)
        let mut children_of: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        // task_id → [blocker_id, ...]
        let mut blockers_of: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for (task, blocker) in &edges {
            children_of.entry(*blocker).or_default().push(*task);
            blockers_of.entry(*task).or_default().push(*blocker);
        }

        // Tasks keyed by id for O(1) lookup during walk.
        let by_id: BTreeMap<Uuid, &Task> = open.iter().map(|t| (t.task_id, t)).collect();

        // Roots: anything that is an epic OR has no unclosed blocker.
        // Epics live as roots even if they have no deps (they bundle work).
        let mut roots: Vec<&Task> = open.iter()
            .filter(|t| {
                let no_blockers = blockers_of.get(&t.task_id)
                    .map(|bs| bs.iter().all(|b| !by_id.contains_key(b)))
                    .unwrap_or(true);
                matches!(t.kind, TaskKind::Epic) || no_blockers
            })
            .collect();
        roots.sort_by_key(|t| (t.priority, t.seq));

        // DFS from each root, emitting RenderRows. Cycle-safe via visited set.
        let mut rows: Vec<RenderRow> = Vec::new();
        let mut visited: HashSet<Uuid> = HashSet::new();
        for r in &roots {
            walk(
                r.task_id, 0, true, &children_of, &by_id, &mut visited, &mut rows,
            );
        }

        // Any task we never visited (cycle orphans etc.) — still show them so
        // nothing goes missing.
        for t in &open {
            if !visited.contains(&t.task_id) {
                let n_children = children_of.get(&t.task_id).map(|v| v.len()).unwrap_or(0);
                rows.push(RenderRow { task: t.clone(), depth: 0, is_root: true, n_children });
                visited.insert(t.task_id);
            }
        }

        self.rows = rows;

        // Fallback — if this repo has no tasks yet, show the last 30 conversation
        // nodes for the agent matching this repo name. Keeps the pane useful
        // even when the user hasn't created any tasks yet.
        self.fallback.clear();
        if self.rows.is_empty() {
            let agent_name = repo.name.clone();
            let convo: Vec<(String, serde_json::Value, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
                r#"SELECT n.kind::text, n.content, n.created_at
                   FROM nodes n JOIN agents a ON a.agent_id = n.agent_id
                   WHERE a.agent_name = $1
                   ORDER BY n.created_at DESC LIMIT 30"#
            ).bind(&agent_name).fetch_all(pool).await.unwrap_or_default();
            for (kind, content, ts) in convo.into_iter().rev() {
                let snippet = content.get("text")
                    .or_else(|| content.get("directive"))
                    .or_else(|| content.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let age_secs = (chrono::Utc::now() - ts).num_seconds();
                self.fallback.push(FallbackRow::Convo { kind, snippet, age_secs });
            }
        }

        let visible_len = if self.rows.is_empty() { self.fallback.len() } else { self.rows.len() };
        if visible_len == 0 {
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= visible_len {
            self.state.select(Some(0));
        }
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " Task graph — {}  ({} open, epics at top)  ",
            self.repo_name, self.rows.len()
        );

        if self.rows.is_empty() {
            // Fallback: recent conversation nodes for this repo's agent.
            if self.fallback.is_empty() {
                let list = List::new(vec![ListItem::new(
                    "No tasks and no conversation yet. Try `ygg task create` or use Claude Code in this repo."
                )])
                .block(Block::default().borders(Borders::ALL).title(title));
                frame.render_widget(list, area);
                return;
            }
            let items: Vec<ListItem> = self.fallback.iter().map(|r| {
                let FallbackRow::Convo { kind, snippet, age_secs } = r;
                let (color, glyph) = match kind.as_str() {
                    "user_message"      => (Color::Cyan,    "●"),
                    "assistant_message" => (Color::Green,   "◉"),
                    "tool_call"         => (Color::Yellow,  "⚙"),
                    "tool_result"       => (Color::DarkGray,"↳"),
                    "digest"            => (Color::Magenta, "◈"),
                    "directive"         => (Color::Blue,    "♦"),
                    _                   => (Color::White,   "◇"),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{glyph} {kind:<18}"), Style::default().fg(color)),
                    Span::styled(format!(" {:>7} ago  ", human_age(*age_secs)),
                        Style::default().fg(Color::DarkGray)),
                    Span::raw(truncate(snippet, 80)),
                ]))
            }).collect();
            let title = format!(" Conversation — {} (no tasks yet; showing recent nodes) ", self.repo_name);
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, area, &mut self.state);
            return;
        }

        let items: Vec<ListItem> = self.rows.iter().map(|r| {
            let t = &r.task;
            let indent = "  ".repeat(r.depth);
            let connector = if r.depth == 0 { "" } else { "└─ " };

            // Kind + glyph
            let (kind_color, kind_glyph) = match t.kind {
                TaskKind::Epic    => (Color::Magenta, "◉"),
                TaskKind::Feature => (Color::Cyan,    "✚"),
                TaskKind::Bug     => (Color::Red,     "✗"),
                TaskKind::Chore   => (Color::DarkGray,"·"),
                TaskKind::Task    => (Color::White,   "○"),
            };

            // Status
            let (status_color, status_label) = match t.status {
                TaskStatus::Open        => (Color::Gray,    "open"),
                TaskStatus::InProgress  => (Color::Yellow,  "wip "),
                TaskStatus::Blocked     => (Color::Red,     "blkd"),
                TaskStatus::Closed      => (Color::DarkGray,"done"),
            };

            // Prio color
            let prio_style = match t.priority {
                0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                1 => Style::default().fg(Color::Yellow),
                2 => Style::default().fg(Color::White),
                _ => Style::default().fg(Color::DarkGray),
            };

            let children_badge = if r.n_children > 0 {
                format!(" {}↓", r.n_children)
            } else { String::new() };

            let id = format!("-{}", t.seq);

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
        }).collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

fn walk(
    id: Uuid,
    depth: usize,
    is_root: bool,
    children_of: &HashMap<Uuid, Vec<Uuid>>,
    by_id: &BTreeMap<Uuid, &Task>,
    visited: &mut HashSet<Uuid>,
    rows: &mut Vec<RenderRow>,
) {
    if !visited.insert(id) { return; }
    let Some(task) = by_id.get(&id).copied() else { return; };
    let children = children_of.get(&id).cloned().unwrap_or_default();
    let n_children = children.iter().filter(|c| by_id.contains_key(c)).count();

    rows.push(RenderRow { task: task.clone(), depth, is_root, n_children });

    // Sort children by priority then seq for stable rendering.
    let mut sorted: Vec<&Uuid> = children.iter().filter(|c| by_id.contains_key(c)).collect();
    sorted.sort_by_key(|c| by_id.get(c).map(|t| (t.priority, t.seq)));

    for child in sorted {
        walk(*child, depth + 1, false, children_of, by_id, visited, rows);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn human_age(secs: i64) -> String {
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else if secs < 86400 { format!("{}h", secs / 3600) }
    else { format!("{}d", secs / 86400) }
}
