pub mod report;

/// Token accumulation rate (tokens per second) from timestamped samples.
/// Each sample is `(unix_timestamp_secs, cumulative_token_count)`.
/// Returns 0.0 when fewer than 2 samples or zero time span.
pub fn token_velocity(samples: &[(u64, i32)]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let first = samples.first().unwrap();
    let last = samples.last().unwrap();
    let dt = last.0.saturating_sub(first.0) as f64;
    if dt == 0.0 {
        return 0.0;
    }
    (last.1 - first.1) as f64 / dt
}

/// Fraction of lock acquisitions that resulted in a conflict.
/// Returns 0.0 when there are no acquisitions.
pub fn lock_contention_ratio(acquires: u32, conflicts: u32) -> f64 {
    if acquires == 0 {
        return 0.0;
    }
    conflicts as f64 / acquires as f64
}

/// Composite pressure score (0.0–1.0+) combining context fullness and lock weight.
/// Context fill is `tokens / limit` (clamped 0–1), lock weight adds 0.1 per lock held.
pub fn agent_pressure_score(tokens: i32, limit: usize, lock_count: usize) -> f64 {
    let fill = if limit == 0 {
        0.0
    } else {
        (tokens.max(0) as f64 / limit as f64).min(1.0)
    };
    fill + lock_count as f64 * 0.1
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── token_velocity ──

    #[test]
    fn velocity_empty() {
        assert_eq!(token_velocity(&[]), 0.0);
    }

    #[test]
    fn velocity_single_sample() {
        assert_eq!(token_velocity(&[(100, 500)]), 0.0);
    }

    #[test]
    fn velocity_same_timestamp() {
        assert_eq!(token_velocity(&[(10, 0), (10, 100)]), 0.0);
    }

    #[test]
    fn velocity_normal() {
        let samples = vec![(0, 0), (10, 1000)];
        assert!((token_velocity(&samples) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn velocity_multiple_samples() {
        // Only first and last matter
        let samples = vec![(0, 0), (5, 200), (20, 400)];
        assert!((token_velocity(&samples) - 20.0).abs() < f64::EPSILON);
    }

    // ── lock_contention_ratio ──

    #[test]
    fn contention_zero_acquires() {
        assert_eq!(lock_contention_ratio(0, 0), 0.0);
    }

    #[test]
    fn contention_no_conflicts() {
        assert_eq!(lock_contention_ratio(10, 0), 0.0);
    }

    #[test]
    fn contention_half() {
        assert!((lock_contention_ratio(10, 5) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn contention_all_conflict() {
        assert!((lock_contention_ratio(4, 4) - 1.0).abs() < f64::EPSILON);
    }

    // ── agent_pressure_score ──

    #[test]
    fn pressure_zero_limit() {
        assert_eq!(agent_pressure_score(100, 0, 0), 0.0);
    }

    #[test]
    fn pressure_empty() {
        assert_eq!(agent_pressure_score(0, 1000, 0), 0.0);
    }

    #[test]
    fn pressure_half_full_no_locks() {
        assert!((agent_pressure_score(500, 1000, 0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn pressure_full_with_locks() {
        // 1.0 fill + 2 locks * 0.1 = 1.2
        assert!((agent_pressure_score(1000, 1000, 2) - 1.2).abs() < f64::EPSILON);
    }

    #[test]
    fn pressure_negative_tokens() {
        assert_eq!(agent_pressure_score(-50, 1000, 0), 0.0);
    }

    #[test]
    fn pressure_over_limit_clamped() {
        // tokens > limit → fill clamped to 1.0
        assert!((agent_pressure_score(2000, 1000, 0) - 1.0).abs() < f64::EPSILON);
    }
}
