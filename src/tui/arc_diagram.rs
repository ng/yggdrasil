//! DAG arc diagram (yggdrasil-162). Tasks laid out left-to-right by
//! topological depth (= longest path from any root); deps drawn as
//! arcs above the row, blockers below. Reveals fan-in bottlenecks
//! ("everything blocks on yggdrasil-82") that a tree view buries.
//!
//! This module owns the layout math: longest-path depth assignment +
//! arc-thickness binning. The renderer (box-drawing arcs at varying
//! heights) layers on top.

use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagNode {
    pub task_id: Uuid,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagEdge {
    pub from: Uuid,
    pub to: Uuid,
}

/// Position of one task on the rendered axis: depth = how many edges
/// must be crossed from any root to reach this node (longest path);
/// column = pixel offset, derived from depth + insertion order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodePosition {
    pub depth: u32,
    pub column: u32,
}

/// Compute longest-path depth for every node. Roots (no incoming
/// edges) sit at depth 0; everything else is `1 + max(depth(parents))`.
/// A cycle would prevent termination — the function bails out and
/// returns whatever it managed to assign so the renderer can still
/// surface partial state. Cycle detection elsewhere (yggdrasil-122)
/// catches those before they reach this layer.
pub fn longest_path_depths(nodes: &[DagNode], edges: &[DagEdge]) -> HashMap<Uuid, u32> {
    let mut depths: HashMap<Uuid, u32> = HashMap::new();
    let parents_of: HashMap<Uuid, Vec<Uuid>> = {
        let mut m: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in edges {
            m.entry(e.to).or_default().push(e.from);
        }
        m
    };

    let mut to_visit: Vec<Uuid> = nodes.iter().map(|n| n.task_id).collect();
    let mut last_progress = to_visit.len() + 1;
    while !to_visit.is_empty() && last_progress != to_visit.len() {
        last_progress = to_visit.len();
        to_visit.retain(|id| {
            let parents = parents_of.get(id);
            let max_parent = match parents {
                None => Some(0),
                Some(ps) => {
                    let mut best: Option<u32> = Some(0);
                    for p in ps {
                        match depths.get(p) {
                            Some(d) => best = Some(best.unwrap().max(d + 1)),
                            None => {
                                best = None;
                                break;
                            }
                        }
                    }
                    best
                }
            };
            match max_parent {
                Some(d) => {
                    depths.insert(*id, d);
                    false
                }
                None => true,
            }
        });
    }
    depths
}

/// Assign columns by depth: every node at depth N gets a column that
/// keeps the layout left-to-right. Columns within a depth tier are
/// stable in the input order so a refresh doesn't reshuffle.
pub fn assign_positions(
    nodes: &[DagNode],
    depths: &HashMap<Uuid, u32>,
) -> HashMap<Uuid, NodePosition> {
    let mut positions: HashMap<Uuid, NodePosition> = HashMap::new();
    let mut tier_offset: HashMap<u32, u32> = HashMap::new();
    for n in nodes {
        let d = depths.get(&n.task_id).copied().unwrap_or(0);
        let col = tier_offset.entry(d).or_insert(0);
        positions.insert(
            n.task_id,
            NodePosition {
                depth: d,
                column: *col,
            },
        );
        *col += 1;
    }
    positions
}

/// Critical path — longest chain by depth count. Returns the task IDs
/// in path order. Used by the renderer to bold-style the arcs along
/// this chain so the eye finds it instantly.
pub fn critical_path(nodes: &[DagNode], edges: &[DagEdge]) -> Vec<Uuid> {
    let depths = longest_path_depths(nodes, edges);
    let parents_of: HashMap<Uuid, Vec<Uuid>> = {
        let mut m: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for e in edges {
            m.entry(e.to).or_default().push(e.from);
        }
        m
    };
    // Walk back from the deepest node. Ties broken by node order.
    let deepest = depths.iter().max_by_key(|(_, d)| **d).map(|(id, _)| *id);
    let Some(start) = deepest else {
        return Vec::new();
    };
    let mut path: Vec<Uuid> = vec![start];
    let mut cur = start;
    let mut visited: HashSet<Uuid> = HashSet::new();
    visited.insert(cur);
    while let Some(parents) = parents_of.get(&cur) {
        if let Some(next) = parents
            .iter()
            .filter(|p| !visited.contains(*p))
            .max_by_key(|p| depths.get(*p).copied().unwrap_or(0))
        {
            path.push(*next);
            visited.insert(*next);
            cur = *next;
        } else {
            break;
        }
    }
    path.reverse();
    path
}
