//! Command palette (yggdrasil-127). Ctrl-K opens a centered overlay
//! with a fuzzy search over every action the TUI exposes (switch
//! pane, save view, open detail, ...). Selecting an action runs its
//! handler and closes the palette. Esc closes without running.
//!
//! Substrate first: this module ships the action registry, the
//! fuzzy-rank logic, and the input/selection state machine. Pane
//! integration (the Ctrl-K key + the per-action handlers) layers in
//! once the renderer's overlay shape is settled.

use tui_input::Input;

/// One action the palette can run. The handler is captured at
/// registration time so the palette doesn't need a reference to App
/// to dispatch — the caller pumps `Action::Handler` through its
/// existing dispatcher when `commit` returns one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Short identifier the palette ranks against. Two-word verb-noun
    /// works best ("switch dashboard", "save view"); keep it lowercase
    /// so the matcher is case-insensitive without re-allocating.
    pub label: String,
    /// One-line right-aligned hint shown next to the label. Optional;
    /// empty string means "no hint."
    pub hint: String,
    /// Stable id for the dispatcher. Caller's match arm picks behaviour
    /// based on this string, not on the label (which is user-facing
    /// and may change).
    pub id: String,
}

/// Built-in actions the palette ships with. Pane authors append
/// pane-specific actions at registration time so the catalog grows
/// without touching this file.
pub fn default_actions() -> Vec<Action> {
    vec![
        Action {
            id: "switch:dashboard".into(),
            label: "switch to dashboard".into(),
            hint: "[1]".into(),
        },
        Action {
            id: "switch:dag".into(),
            label: "switch to dag".into(),
            hint: "[2]".into(),
        },
        Action {
            id: "switch:tasks".into(),
            label: "switch to tasks".into(),
            hint: "[3]".into(),
        },
        Action {
            id: "switch:trace".into(),
            label: "switch to trace".into(),
            hint: "[4]".into(),
        },
        Action {
            id: "switch:query".into(),
            label: "switch to query".into(),
            hint: "[5]".into(),
        },
        Action {
            id: "switch:logs".into(),
            label: "switch to logs".into(),
            hint: "[6]".into(),
        },
        Action {
            id: "switch:memgraph".into(),
            label: "switch to memgraph".into(),
            hint: "[7]".into(),
        },
        Action {
            id: "switch:eval".into(),
            label: "switch to eval".into(),
            hint: "[8]".into(),
        },
        Action {
            id: "switch:prompt".into(),
            label: "switch to prompt".into(),
            hint: "[9]".into(),
        },
        Action {
            id: "switch:locks".into(),
            label: "switch to locks".into(),
            hint: "[0]".into(),
        },
        Action {
            id: "switch:runs".into(),
            label: "switch to runs".into(),
            hint: "[R]".into(),
        },
        Action {
            id: "switch:run-grid".into(),
            label: "switch to run grid".into(),
            hint: "[G]".into(),
        },
        Action {
            id: "scope:toggle".into(),
            label: "toggle scope (repo / all)".into(),
            hint: "[S]".into(),
        },
        Action {
            id: "help:toggle".into(),
            label: "show help".into(),
            hint: "[?]".into(),
        },
    ]
}

#[derive(Debug, Default)]
pub struct Palette {
    pub open: bool,
    pub input: Input,
    pub selected: usize,
    actions: Vec<Action>,
}

impl Palette {
    pub fn new(actions: Vec<Action>) -> Self {
        Self {
            open: false,
            input: Input::default(),
            selected: 0,
            actions,
        }
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.input = Input::default();
            self.selected = 0;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input = Input::default();
        self.selected = 0;
    }

    pub fn move_down(&mut self) {
        let n = self.matches().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Commit the current selection. Returns the action id the caller
    /// should dispatch, or None when there's nothing matching.
    pub fn commit(&mut self) -> Option<String> {
        let id = self.matches().get(self.selected).map(|a| a.id.clone());
        if id.is_some() {
            self.close();
        }
        id
    }

    /// Live-filtered actions ordered by descending score. Empty
    /// pattern returns every action in registration order so the
    /// catalog reads naturally before the user types anything.
    pub fn matches(&self) -> Vec<&Action> {
        let pat = self.input.value().to_lowercase();
        if pat.is_empty() {
            return self.actions.iter().collect();
        }
        let mut scored: Vec<(i32, &Action)> = self
            .actions
            .iter()
            .filter_map(|a| score(&pat, &a.label).map(|s| (s, a)))
            .collect();
        // Higher score → earlier; ties broken by original registration
        // order via a stable sort.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, a)| a).collect()
    }
}

/// Tiny in-house fuzzy ranker. Scores higher for:
///   - exact prefix matches  (+100)
///   - contiguous substring  (+50)
///   - characters in order   (+1 per matched char)
///
/// Returns None when the pattern isn't a subsequence of the haystack.
/// Sufficient for ~50 actions; if the catalog grows past a few hundred
/// entries, swap in `nucleo` (already vetted by the foundations PR).
pub fn score(pattern: &str, haystack: &str) -> Option<i32> {
    let h = haystack.to_lowercase();
    if h.starts_with(pattern) {
        return Some(100 + pattern.len() as i32);
    }
    if h.contains(pattern) {
        return Some(50 + pattern.len() as i32);
    }
    let mut pat_iter = pattern.chars();
    let mut next = pat_iter.next()?;
    let mut hits = 0;
    for c in h.chars() {
        if c == next {
            hits += 1;
            match pat_iter.next() {
                Some(n) => next = n,
                None => return Some(hits),
            }
        }
    }
    None
}
