pub mod agents;
pub mod reap;
pub mod report;
pub mod tmux_probe;
pub mod workers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceScope {
    User(String),
    Global,
}
