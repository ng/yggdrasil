#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceReportEntry {
    pub resource: String,
    pub action: String,
    pub detail: String,
}
