//! Opt-in diagnostic timings. Static phase names and durations only: never
//! document bodies, file paths, session IDs or credentials. Disabled by default.
use std::time::Instant;

pub(crate) struct Phase {
    name: &'static str,
    start: Option<Instant>,
}
impl Phase {
    pub(crate) fn start(name: &'static str) -> Self {
        Self {
            name,
            start: tracing::enabled!(target: "ygg::knowledge::timing", tracing::Level::DEBUG)
                .then(Instant::now),
        }
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            tracing::debug!(target: "ygg::knowledge::timing",
                phase = self.name, elapsed_us = start.elapsed().as_micros() as u64,
                "knowledge phase");
        }
    }
}
