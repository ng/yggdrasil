//! Multi-select state (yggdrasil-129). Generic over a `Key` type so
//! agents (Uuid), tasks (Uuid), and locks (String) can all reuse the
//! same machinery. Space toggles the row under the cursor; `A` selects
//! every visible row; `X` runs the bulk action callback. Per-pane
//! wiring of those keys + the bulk handlers lands incrementally.

use std::collections::BTreeSet;
use std::hash::Hash;

#[derive(Debug, Clone, Default)]
pub struct MultiSelect<Key: Eq + Hash + Ord + Clone> {
    pub selected: BTreeSet<Key>,
}

impl<Key: Eq + Hash + Ord + Clone> MultiSelect<Key> {
    pub fn new() -> Self {
        Self {
            selected: BTreeSet::new(),
        }
    }

    /// Space-bar toggle. Returns whether the key is now selected.
    pub fn toggle(&mut self, key: Key) -> bool {
        if self.selected.contains(&key) {
            self.selected.remove(&key);
            false
        } else {
            self.selected.insert(key);
            true
        }
    }

    /// `A` for select-all over the supplied row keys. Implementation
    /// is "extend, don't replace" so the user can multi-select across
    /// page boundaries — `A` on a filtered subset adds to whatever's
    /// already picked.
    pub fn select_all(&mut self, keys: impl IntoIterator<Item = Key>) {
        self.selected.extend(keys);
    }

    /// Inverse of select_all over the supplied keys. Useful for
    /// "deselect this filter" when the user changes filter pattern.
    pub fn deselect(&mut self, keys: impl IntoIterator<Item = Key>) {
        for k in keys {
            self.selected.remove(&k);
        }
    }

    /// Drop everything (Esc-Esc, pane switch, post-bulk-action).
    pub fn clear(&mut self) {
        self.selected.clear();
    }

    pub fn is_selected(&self, key: &Key) -> bool {
        self.selected.contains(key)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Snapshot the selection as a Vec — used by bulk handlers that
    /// want to iterate stably and need the ordered set guarantee.
    pub fn snapshot(&self) -> Vec<Key> {
        self.selected.iter().cloned().collect()
    }
}
