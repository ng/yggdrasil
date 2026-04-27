//! Memory similarity ridgeline (yggdrasil-164). Per-query KDE of
//! cosine-similarity scores from `similarity_hit` events, stacked by
//! recency. Tall narrow peaks at 0.9+ = strong recall; flat ridges
//! across the [0, 1] range = weak retrieval — the *quality* signal
//! that a "top-5 list" never shows.
//!
//! Substrate first: the KDE binning + ridge smoothing + glyph picker.
//! Renderer (braille curves, recency-tinted color) layers on top.

/// Number of bins along the [0.0, 1.0] similarity axis. 32 bins gives
/// smooth ridges at typical pane widths without over-quantising.
pub const BIN_COUNT: usize = 32;

/// One per-query ridge: `bins[i]` = density at cosine similarity in
/// `[i / BIN_COUNT, (i+1) / BIN_COUNT]`. The renderer normalises each
/// ridge against its own peak so weak-recall queries stay legible.
#[derive(Debug, Clone, PartialEq)]
pub struct Ridge {
    pub label: String,
    pub bins: Vec<u32>,
}

/// Bin a slice of similarity scores into a `Ridge`. Scores outside
/// `[0.0, 1.0]` get clamped — we don't drop them silently because
/// distance metrics occasionally drift slightly above 1 due to
/// floating-point error.
pub fn bin_ridge(label: impl Into<String>, scores: &[f64]) -> Ridge {
    let mut bins = vec![0u32; BIN_COUNT];
    for &s in scores {
        let clamped = s.clamp(0.0, 1.0);
        // floor not round so 1.0 still lands in the last bin.
        let mut idx = (clamped * BIN_COUNT as f64) as usize;
        if idx >= BIN_COUNT {
            idx = BIN_COUNT - 1;
        }
        bins[idx] = bins[idx].saturating_add(1);
    }
    Ridge {
        label: label.into(),
        bins,
    }
}

/// Eight-step Unicode block heights for a single ridge cell. Index 0
/// = empty (nothing in this bin); 8 = full crest at the ridge's max.
pub const RIDGE_LEVELS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render one ridge's bins to a glyph string. Each bin maps to one
/// cell width-wise; height encoded by the partial-block char. Empty
/// ridges (every bin zero) render as `width` spaces.
pub fn render_ridge(ridge: &Ridge) -> String {
    let max = *ridge.bins.iter().max().unwrap_or(&0);
    if max == 0 {
        return " ".repeat(ridge.bins.len());
    }
    ridge
        .bins
        .iter()
        .map(|&v| {
            let frac = v as f64 / max as f64;
            let idx = (frac * (RIDGE_LEVELS.len() - 1) as f64).round() as usize;
            RIDGE_LEVELS[idx.min(RIDGE_LEVELS.len() - 1)]
        })
        .collect()
}

/// Quality classification — mostly for the renderer's color tint. A
/// tall peak above 0.85 (the top 15% of similarity space) reads as
/// strong recall; flat distributions read as weak/diluted retrieval.
pub fn quality_class(ridge: &Ridge) -> RidgeQuality {
    let total: u32 = ridge.bins.iter().sum();
    if total == 0 {
        return RidgeQuality::Empty;
    }
    let high_band: u32 = ridge.bins[(BIN_COUNT * 85 / 100)..].iter().sum();
    let high_frac = high_band as f64 / total as f64;
    if high_frac >= 0.5 {
        RidgeQuality::Strong
    } else if high_frac >= 0.2 {
        RidgeQuality::Mixed
    } else {
        RidgeQuality::Weak
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RidgeQuality {
    Empty,
    Strong,
    Mixed,
    Weak,
}
