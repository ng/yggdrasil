//! Scenario registry. Each scenario describes a fixture DAG and a deterministic
//! grader. The drivers (vanilla-single, vanilla-tmux, ygg) are scenario-agnostic
//! — they read the manifest and execute. See docs/eval-benchmarks.md.
//!
//! For now this is a static registry. Ship Scenario 1 first (yggdrasil-103);
//! 2–6 land incrementally.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ScenarioSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Default parallelism if not overridden on the CLI.
    pub default_parallelism: u32,
    /// Whether the scenario is implemented end-to-end (registry includes
    /// stubs for the others as a roadmap signal).
    pub implemented: bool,
}

/// All known scenarios, ordered by index in docs/eval-benchmarks.md.
pub const ALL: &[ScenarioSpec] = &[
    ScenarioSpec {
        id: "independent-parallel-n",
        title: "Independent parallel — throughput baseline",
        description: "N independent doc-page tasks, no shared files. Measures coordination overhead.",
        default_parallelism: 4,
        implemented: false,
    },
    ScenarioSpec {
        id: "dag-linear-3",
        title: "Linear DAG — sequencing correctness",
        description: "A → B → C with strict ordering. Measures dependency enforcement.",
        default_parallelism: 1,
        implemented: false,
    },
    ScenarioSpec {
        id: "fan-out-fan-in",
        title: "Fan-out/fan-in — aggregation",
        description: "Root spawns N children; aggregator verifies coherent merge.",
        default_parallelism: 3,
        implemented: false,
    },
    ScenarioSpec {
        id: "contention",
        title: "Lock contention",
        description: "Two tasks both edit the same file. Measures lock correctness.",
        default_parallelism: 2,
        implemented: false,
    },
    ScenarioSpec {
        id: "failure-recovery",
        title: "Failure recovery",
        description: "Inject SIGKILL mid-run; measure scheduler reap + retry.",
        default_parallelism: 2,
        implemented: false,
    },
    ScenarioSpec {
        id: "long-horizon-refactor",
        title: "Long-horizon refactor — METR-style",
        description: "Multi-file rename. Anchor for the 50%-time-horizon curve.",
        default_parallelism: 1,
        implemented: false,
    },
];

pub fn find(id: &str) -> Option<&'static ScenarioSpec> {
    ALL.iter().find(|s| s.id == id)
}

/// Conventional location for scenario fixtures (manifest, seed_repo, grade.sh).
/// Returns the directory whether it exists or not — callers handle missing dirs.
pub fn fixture_dir(id: &str) -> PathBuf {
    PathBuf::from("benches/scenarios").join(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for s in ALL {
            assert!(seen.insert(s.id), "duplicate scenario id: {}", s.id);
        }
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("not-a-scenario").is_none());
        assert!(find("independent-parallel-n").is_some());
    }

    #[test]
    fn fixture_path_relative() {
        let p = fixture_dir("contention");
        assert_eq!(p, PathBuf::from("benches/scenarios/contention"));
    }
}
