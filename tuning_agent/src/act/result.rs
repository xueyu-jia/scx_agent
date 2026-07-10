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
    pub rollback_performed: bool,
}
