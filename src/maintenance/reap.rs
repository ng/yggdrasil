#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapWindow {
    pub older_than_days: i64,
}
