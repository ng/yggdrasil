use uuid::Uuid;

/// Salience decay curve for directive injection (from agent-ways patterns).
///
/// Each directive has a salience score that decays with token distance
/// from the attention cursor. The governor limits concurrent injections
/// to prevent attention saturation.
#[derive(Debug, Clone)]
pub struct SalienceConfig {
    /// Maximum concurrent directive injections per prompt.
    pub max_concurrent: usize,
    /// Minimum salience score to include a directive (0.0 - 1.0).
    pub floor: f64,
    /// Half-life in tokens — salience drops to 50% at this distance.
    pub half_life_tokens: usize,
}

impl Default for SalienceConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            floor: 0.1,
            half_life_tokens: 50_000,
        }
    }
}

/// A directive with its current salience score.
#[derive(Debug, Clone)]
pub struct ScoredDirective {
    pub node_id: Uuid,
    pub content: String,
    pub token_count: i32,
    /// Raw similarity score from pgvector (0.0 - 1.0, higher = more similar).
    pub similarity: f64,
    /// Token distance from the current cursor position.
    pub token_distance: usize,
    /// Final salience score after decay.
    pub salience: f64,
}

/// Injection governor — filters and ranks directives by salience.
pub struct Governor {
    config: SalienceConfig,
    /// Session-level dedup: directive node IDs already injected this session.
    seen: std::collections::HashSet<Uuid>,
}

impl Governor {
    pub fn new(config: SalienceConfig) -> Self {
        Self {
            config,
            seen: std::collections::HashSet::new(),
        }
    }

    /// Calculate salience for a directive given its similarity score and token distance.
    /// Uses exponential decay: salience = similarity * 2^(-distance / half_life)
    pub fn calculate_salience(&self, similarity: f64, token_distance: usize) -> f64 {
        let decay = (-1.0 * token_distance as f64 / self.config.half_life_tokens as f64).exp2();
        similarity * decay
    }

    /// Filter and rank directives. Returns at most `max_concurrent` directives
    /// above the salience floor, excluding already-seen ones this session.
    pub fn govern(&mut self, mut directives: Vec<ScoredDirective>) -> Vec<ScoredDirective> {
        // Remove already-seen directives (once-per-session dedup)
        directives.retain(|d| !self.seen.contains(&d.node_id));

        // Remove below-floor
        directives.retain(|d| d.salience >= self.config.floor);

        // Sort by salience descending
        directives.sort_by(|a, b| {
            b.salience
                .partial_cmp(&a.salience)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Cap at max_concurrent
        directives.truncate(self.config.max_concurrent);

        // Mark as seen
        for d in &directives {
            self.seen.insert(d.node_id);
        }

        directives
    }

    /// Reset session markers (call on context compaction/auto-compact).
    pub fn reset_session(&mut self) {
        self.seen.clear();
    }

    /// Check how many directives have been injected this session.
    pub fn session_count(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_salience_decay() {
        let gov = Governor::new(SalienceConfig::default());

        // At distance 0, salience = similarity
        assert!((gov.calculate_salience(0.9, 0) - 0.9).abs() < 0.001);

        // At half_life distance, salience = similarity * 0.5
        assert!((gov.calculate_salience(0.9, 50_000) - 0.45).abs() < 0.001);

        // At 2x half_life, salience = similarity * 0.25
        assert!((gov.calculate_salience(0.9, 100_000) - 0.225).abs() < 0.001);
    }

    #[test]
    fn test_governor_caps_and_dedup() {
        let mut gov = Governor::new(SalienceConfig {
            max_concurrent: 2,
            floor: 0.05,
            half_life_tokens: 50_000,
        });

        let directives = vec![
            ScoredDirective {
                node_id: Uuid::new_v4(),
                content: "a".into(),
                token_count: 10,
                similarity: 0.9,
                token_distance: 0,
                salience: 0.9,
            },
            ScoredDirective {
                node_id: Uuid::new_v4(),
                content: "b".into(),
                token_count: 10,
                similarity: 0.8,
                token_distance: 0,
                salience: 0.8,
            },
            ScoredDirective {
                node_id: Uuid::new_v4(),
                content: "c".into(),
                token_count: 10,
                similarity: 0.7,
                token_distance: 0,
                salience: 0.7,
            },
        ];

        let result = gov.govern(directives);
        assert_eq!(result.len(), 2); // capped at max_concurrent
        assert!(result[0].salience > result[1].salience); // sorted

        // Second call — already-seen directives are deduped
        let more = vec![ScoredDirective {
            node_id: result[0].node_id,
            content: "a".into(),
            token_count: 10,
            similarity: 0.9,
            token_distance: 0,
            salience: 0.9,
        }];
        let result2 = gov.govern(more);
        assert_eq!(result2.len(), 0); // deduped
    }
}
