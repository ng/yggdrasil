//! Bootstrap CIs and pass^k math for `ygg bench report` / `diff`.
//! See docs/eval-benchmarks.md § Reliability strategy.

/// 95% bootstrap CI of the mean using `n_resamples` resamples. Cheap and
/// non-parametric; fits pass-rate and wall-clock data equally well. Returns
/// (mean, lower_95, upper_95).
pub fn bootstrap_mean_ci(samples: &[f64], n_resamples: usize) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if samples.len() == 1 {
        return (mean, mean, mean);
    }

    let mut means = Vec::with_capacity(n_resamples);
    let mut state = 0xdeadbeef_u64;
    for _ in 0..n_resamples {
        let mut sum = 0.0;
        for _ in 0..samples.len() {
            state = next_u64(state);
            let idx = (state as usize) % samples.len();
            sum += samples[idx];
        }
        means.push(sum / samples.len() as f64);
    }
    means.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo = percentile(&means, 0.025);
    let hi = percentile(&means, 0.975);
    (mean, lo, hi)
}

/// Pass^k = probability all k attempts pass. Computed as the fraction of
/// successive groups of size k where every element is true. Returns
/// `passes / total_groups`. tau2-bench's headline reliability metric.
pub fn pass_power_k(passes: &[bool], k: usize) -> f64 {
    if k == 0 || passes.len() < k {
        return 0.0;
    }
    let groups = passes.len() / k;
    let all_pass = passes.chunks_exact(k).filter(|g| g.iter().all(|&p| p)).count();
    all_pass as f64 / groups as f64
}

/// Pass@k — probability at least one attempt out of k passes. The optimistic
/// twin of pass^k.
pub fn pass_at_k(passes: &[bool], k: usize) -> f64 {
    if k == 0 || passes.len() < k {
        return 0.0;
    }
    let groups = passes.len() / k;
    let any_pass = passes.chunks_exact(k).filter(|g| g.iter().any(|&p| p)).count();
    any_pass as f64 / groups as f64
}

fn next_u64(s: u64) -> u64 {
    let mut x = s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x.wrapping_add(0x9E3779B97F4A7C15)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Confidence-interval-overlap test. The bench-diff guard: refuse to declare
/// a winner when the 95% CIs of two samples overlap. Returns:
/// - `Verdict::Inconclusive` when CIs overlap
/// - `Verdict::ALess` when a's upper < b's lower
/// - `Verdict::AMore` when a's lower > b's upper
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict { Inconclusive, ALess, AMore }

pub fn ci_diff_verdict(
    a_lo: f64, a_hi: f64,
    b_lo: f64, b_hi: f64,
) -> Verdict {
    if a_hi < b_lo { Verdict::ALess }
    else if a_lo > b_hi { Verdict::AMore }
    else { Verdict::Inconclusive }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_power_k_all_pass() {
        let p = vec![true, true, true, true];
        assert_eq!(pass_power_k(&p, 4), 1.0);
        assert_eq!(pass_at_k(&p, 4), 1.0);
    }

    #[test]
    fn pass_power_k_one_fails() {
        let p = vec![true, true, true, false];
        assert_eq!(pass_power_k(&p, 4), 0.0);
        assert_eq!(pass_at_k(&p, 4), 1.0);
    }

    #[test]
    fn pass_power_k_two_groups() {
        let p = vec![true, true, true, false, true, true, true, true];
        assert_eq!(pass_power_k(&p, 4), 0.5);
        assert_eq!(pass_at_k(&p, 4), 1.0);
    }

    #[test]
    fn bootstrap_returns_zero_for_empty() {
        let (m, lo, hi) = bootstrap_mean_ci(&[], 100);
        assert_eq!(m, 0.0);
        assert_eq!(lo, 0.0);
        assert_eq!(hi, 0.0);
    }

    #[test]
    fn bootstrap_collapses_for_one() {
        let (m, lo, hi) = bootstrap_mean_ci(&[42.0], 100);
        assert_eq!(m, 42.0);
        assert_eq!(lo, 42.0);
        assert_eq!(hi, 42.0);
    }

    #[test]
    fn bootstrap_returns_mean_within_ci() {
        let (m, lo, hi) = bootstrap_mean_ci(&[10.0, 12.0, 14.0, 13.0, 11.0], 1000);
        assert!(lo <= m && m <= hi);
        assert!((m - 12.0).abs() < 0.01);
        assert!(lo < hi);
    }

    #[test]
    fn ci_overlap_is_inconclusive() {
        assert_eq!(ci_diff_verdict(10.0, 20.0, 15.0, 25.0), Verdict::Inconclusive);
    }

    #[test]
    fn ci_disjoint_is_decisive() {
        assert_eq!(ci_diff_verdict(10.0, 20.0, 25.0, 30.0), Verdict::ALess);
        assert_eq!(ci_diff_verdict(25.0, 30.0, 10.0, 20.0), Verdict::AMore);
    }
}
