//! Mechanical scoring for `ygg inject` candidates — see ADR 0012.
//!
//! Weighted-sum over features already in the DB: cosine, node kind, age,
//! same-repo, same-agent. Default bias is toward letting candidates through
//! ("rank, don't gate") — the soft cap is `YGG_MECH_MAX_HITS` (default 8)
//! and the floor is `YGG_MECH_MIN_SCORE` (default 0.05).

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::models::node::{NodeKind, SearchHit};

#[derive(Debug, Clone, Serialize)]
pub struct ComponentScores {
    pub cosine: f64,
    pub kind_boost: f64,
    pub age_weight: f64,
    pub repo_weight: f64,
    pub agent_weight: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoredHit {
    pub hit_index: usize,       // index into the original hits slice
    pub scores: ComponentScores,
    pub dropped: bool,
    pub drop_reason: &'static str,
}

pub struct Scorer {
    pub max_hits: usize,
    pub min_score: f64,
    pub age_halflife_days: f64,
    pub cross_repo_weight: f64,
    pub cross_agent_weight: f64,
    pub dedup_cosine_threshold: f64,
}

impl Scorer {
    pub fn from_env() -> Self {
        Self {
            max_hits: env_num("YGG_MECH_MAX_HITS", 8),
            min_score: env_f64("YGG_MECH_MIN_SCORE", 0.05),
            age_halflife_days: env_f64("YGG_MECH_AGE_HALFLIFE_DAYS", 14.0),
            cross_repo_weight: env_f64("YGG_MECH_CROSS_REPO", 0.85),
            cross_agent_weight: env_f64("YGG_MECH_CROSS_AGENT", 0.95),
            dedup_cosine_threshold: env_f64("YGG_MECH_DEDUP_THRESHOLD", 0.92),
        }
    }

    /// Score a batch of hits. Returns one ScoredHit per input hit in the
    /// same order, with `dropped=true` set for:
    ///   - near-duplicates of an already-kept hit (drop_reason="duplicate")
    ///   - scores below the floor (drop_reason="below_floor")
    ///   - anything past the soft cap (drop_reason="over_cap")
    ///
    /// Callers sort by `scores.total` descending when emitting.
    pub fn score(
        &self,
        hits: &[SearchHit],
        current_agent_id: Uuid,
        current_repo_id: Option<Uuid>,
        hit_repo_ids: &[Option<Uuid>], // same len as hits
        now: DateTime<Utc>,
    ) -> Vec<ScoredHit> {
        let mut scored: Vec<ScoredHit> = hits.iter().enumerate().map(|(i, h)| {
            let cosine = h.similarity();
            let kind_boost = kind_boost(&h.kind);
            let age_days = (now - h.created_at).num_seconds() as f64 / 86_400.0;
            let age_weight = (0.5_f64).powf(age_days.max(0.0) / self.age_halflife_days);
            let same_agent = h.agent_id == current_agent_id;
            let agent_weight = if same_agent { 1.0 } else { self.cross_agent_weight };
            let same_repo = match (current_repo_id, hit_repo_ids.get(i).copied().flatten()) {
                (Some(a), Some(b)) => a == b,
                _ => true, // no repo context either way — neutral
            };
            let repo_weight = if same_repo { 1.0 } else { self.cross_repo_weight };

            let total = cosine * kind_boost * age_weight * repo_weight * agent_weight;

            ScoredHit {
                hit_index: i,
                scores: ComponentScores {
                    cosine, kind_boost, age_weight, repo_weight, agent_weight, total,
                },
                dropped: false,
                drop_reason: "",
            }
        }).collect();

        // Mark near-duplicates (kept first-seen). O(k^2) but k is small (<=20).
        // We dedupe on snippet text equality — a stricter version would use
        // cosine between candidate embeddings, but we don't have those here.
        let mut seen_snippets: Vec<String> = Vec::with_capacity(scored.len());
        for (i, h) in hits.iter().enumerate() {
            if scored[i].dropped { continue; }
            let snippet = snippet_key(&h.content);
            if seen_snippets.iter().any(|s| s == &snippet) {
                scored[i].dropped = true;
                scored[i].drop_reason = "duplicate";
            } else {
                seen_snippets.push(snippet);
            }
        }

        // Sort by total score descending (preserving dropped flags).
        // We sort a permutation so we can still return in original order.
        let mut order: Vec<usize> = (0..scored.len()).collect();
        order.sort_by(|&a, &b| scored[b].scores.total.partial_cmp(&scored[a].scores.total).unwrap_or(std::cmp::Ordering::Equal));

        // Walk the sorted order; keep up to max_hits non-dropped, drop the rest.
        let mut kept = 0usize;
        for rank_pos in &order {
            let i = *rank_pos;
            if scored[i].dropped { continue; }
            if scored[i].scores.total < self.min_score {
                scored[i].dropped = true;
                scored[i].drop_reason = "below_floor";
                continue;
            }
            if kept >= self.max_hits {
                scored[i].dropped = true;
                scored[i].drop_reason = "over_cap";
                continue;
            }
            kept += 1;
        }

        scored
    }
}

fn kind_boost(k: &NodeKind) -> f64 {
    match k {
        NodeKind::Directive        => 1.3,
        NodeKind::Digest           => 1.2,
        NodeKind::HumanOverride    => 1.15,
        NodeKind::UserMessage      => 1.0,
        NodeKind::AssistantMessage => 0.9,
        NodeKind::System           => 0.85,
        NodeKind::ToolCall         => 0.8,
        NodeKind::ToolResult       => 0.75,
    }
}

fn snippet_key(content: &serde_json::Value) -> String {
    let text = content.get("text")
        .or_else(|| content.get("directive"))
        .or_else(|| content.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Normalize whitespace + case so near-duplicates collapse.
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .chars()
        .take(160)
        .collect()
}

fn env_f64(k: &str, default: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_num(k: &str, default: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: Uuid, agent: Uuid, kind: NodeKind, distance: f64, text: &str, created: DateTime<Utc>) -> SearchHit {
        SearchHit {
            id,
            agent_id: agent,
            agent_name: "test".into(),
            kind,
            content: serde_json::json!({ "text": text }),
            token_count: 10,
            created_at: created,
            distance,
        }
    }

    #[test]
    fn fresh_digest_beats_old_tool_result() {
        let s = Scorer::from_env();
        let agent = Uuid::new_v4();
        let now = Utc::now();
        let hits = vec![
            hit(Uuid::new_v4(), agent, NodeKind::Digest, 0.3, "recent digest", now),
            hit(Uuid::new_v4(), agent, NodeKind::ToolResult, 0.3, "old tool output", now - chrono::Duration::days(30)),
        ];
        let scored = s.score(&hits, agent, None, &[None, None], now);
        assert!(scored[0].scores.total > scored[1].scores.total);
    }

    #[test]
    fn cap_caps() {
        let s = Scorer { max_hits: 2, ..Scorer::from_env() };
        let agent = Uuid::new_v4();
        let now = Utc::now();
        let hits: Vec<_> = (0..5).map(|i| {
            hit(Uuid::new_v4(), agent, NodeKind::UserMessage, 0.2 + (i as f64) * 0.1, &format!("q{i}"), now)
        }).collect();
        let repo_ids = vec![None; hits.len()];
        let scored = s.score(&hits, agent, None, &repo_ids, now);
        let kept = scored.iter().filter(|h| !h.dropped).count();
        assert_eq!(kept, 2);
    }

    #[test]
    fn dedupes_identical_text() {
        let s = Scorer::from_env();
        let agent = Uuid::new_v4();
        let now = Utc::now();
        let hits = vec![
            hit(Uuid::new_v4(), agent, NodeKind::Directive, 0.3, "exact same text", now),
            hit(Uuid::new_v4(), agent, NodeKind::Directive, 0.3, "exact same text", now),
        ];
        let scored = s.score(&hits, agent, None, &[None, None], now);
        let dups: Vec<_> = scored.iter().filter(|h| h.drop_reason == "duplicate").collect();
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn below_floor_is_dropped() {
        let s = Scorer { min_score: 0.5, ..Scorer::from_env() };
        let agent = Uuid::new_v4();
        let now = Utc::now();
        // distance 0.99 → similarity 0.01 → total tiny
        let hits = vec![hit(Uuid::new_v4(), agent, NodeKind::UserMessage, 0.99, "hi", now)];
        let scored = s.score(&hits, agent, None, &[None], now);
        assert!(scored[0].dropped);
        assert_eq!(scored[0].drop_reason, "below_floor");
    }
}
