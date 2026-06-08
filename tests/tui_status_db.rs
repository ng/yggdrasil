//! Regression for the DB-side fields on OpsStats (yggdrasil-177).
//! No DB connection here — verifies the struct shape + defaults so
//! a future field reorder doesn't silently break the renderer.

use ygg::tui::app::OpsStats;

#[test]
fn db_side_fields_default_to_zero() {
    let s = OpsStats::default();
    assert_eq!(s.pool_used, 0);
    assert_eq!(s.pool_max, 0);
    assert_eq!(s.events_per_min, 0);
    assert_eq!(s.db_ms, 0);
}

#[test]
fn ops_stats_partial_eq_includes_new_fields() {
    let mut a = OpsStats::default();
    let b = OpsStats::default();
    assert_eq!(a, b);
    a.pool_used = 4;
    assert_ne!(a, b, "moving pool_used should bust equality");
    a.pool_used = 0;
    a.db_ms = 7;
    assert_ne!(a, b, "moving db_ms should bust equality");
    a.db_ms = 0;
    a.events_per_min = 9;
    assert_ne!(a, b, "moving events_per_min should bust equality");
}
