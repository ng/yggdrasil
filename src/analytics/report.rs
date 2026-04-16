use super::{agent_pressure_score, lock_contention_ratio, token_velocity};

/// Build a markdown report from raw inputs.
pub fn format_report(
    samples: &[(u64, i32)],
    acquires: u32,
    conflicts: u32,
    tokens: i32,
    limit: usize,
    lock_count: usize,
) -> String {
    let velocity = token_velocity(samples);
    let contention = lock_contention_ratio(acquires, conflicts);
    let pressure = agent_pressure_score(tokens, limit, lock_count);

    let pressure_indicator = match pressure {
        p if p < 0.5 => "low",
        p if p < 0.8 => "moderate",
        p if p < 1.0 => "high",
        _ => "critical",
    };

    format!(
        "## Analytics Report\n\
         \n\
         | Metric | Value |\n\
         |--------|-------|\n\
         | Token velocity | {velocity:.1} tok/s |\n\
         | Lock contention | {contention_pct:.1}% |\n\
         | Pressure score | {pressure:.2} ({pressure_indicator}) |\n",
        contention_pct = contention * 100.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_contains_all_metrics() {
        let report = format_report(&[(0, 0), (10, 500)], 20, 4, 125_000, 250_000, 1);
        assert!(report.contains("Token velocity"));
        assert!(report.contains("50.0 tok/s"));
        assert!(report.contains("20.0%"));
        assert!(report.contains("0.60"));
        assert!(report.contains("moderate"));
    }

    #[test]
    fn report_empty_inputs() {
        let report = format_report(&[], 0, 0, 0, 0, 0);
        assert!(report.contains("0.0 tok/s"));
        assert!(report.contains("0.0%"));
        assert!(report.contains("0.00"));
        assert!(report.contains("low"));
    }
}
