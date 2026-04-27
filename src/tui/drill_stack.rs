//! Drill stack with breadcrumbs (yggdrasil-133). When the user drills
//! Tasks → Task detail → Run detail, the stack records each step so
//! Backspace pops back to the prior context. Breadcrumbs render in the
//! help row so the user always knows where they are.
//!
//! The stack is bounded to keep accidental Enter-spamming from
//! growing it without bound; depth >= MAX_DEPTH overwrites the oldest
//! entry, which never matters in practice but guards against a cursor
//! stuck on a row that re-pushes itself.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillStep {
    /// Stable label used in the breadcrumb ("Tasks", "Task yggdrasil-42",
    /// "Run #2"). Kept short so the chrome row doesn't wrap.
    pub label: String,
    /// Free-form pane payload. Panes encode whatever they need to
    /// restore the prior selection (task_id, run_id, scroll offset).
    pub payload: String,
}

pub const MAX_DEPTH: usize = 16;

#[derive(Debug, Default, Clone)]
pub struct DrillStack {
    pub stack: Vec<DrillStep>,
}

impl DrillStack {
    /// Push a new drill step. At MAX_DEPTH the oldest entry rotates
    /// out so the stack never grows without bound.
    pub fn push(&mut self, step: DrillStep) {
        self.stack.push(step);
        if self.stack.len() > MAX_DEPTH {
            self.stack.remove(0);
        }
    }

    /// Pop the deepest step (Backspace). Returns the popped frame so
    /// the pane can restore its prior cursor / scroll.
    pub fn pop(&mut self) -> Option<DrillStep> {
        self.stack.pop()
    }

    /// Total clear (Esc-Esc or pane switch). Returns whatever was on
    /// the stack so callers can record one event for the whole drop.
    pub fn clear(&mut self) -> Vec<DrillStep> {
        std::mem::take(&mut self.stack)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// Breadcrumb string for the help row. Renders `>` between steps
    /// so it reads "Tasks > yggdrasil-42 > run #2".
    pub fn breadcrumb(&self) -> String {
        if self.stack.is_empty() {
            return String::new();
        }
        self.stack
            .iter()
            .map(|s| s.label.as_str())
            .collect::<Vec<_>>()
            .join(" › ")
    }
}
