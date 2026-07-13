#[derive(Clone, Copy, Debug)]
pub enum ActStatus {
    DryRun,
    Completed,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct ActResult {
    pub status: ActStatus,
    pub message: String,
    pub rollback_required: bool,
    pub rollback_attempted: bool,
    pub rollback_succeeded: Option<bool>,
    pub rollback_error: Option<String>,
}

impl ActResult {
    pub fn without_rollback(status: ActStatus, message: String) -> Self {
        Self {
            status,
            message,
            rollback_required: false,
            rollback_attempted: false,
            rollback_succeeded: None,
            rollback_error: None,
        }
    }
}
