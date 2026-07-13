use crate::evaluate::{EvaluationDecision, EvaluationPlan};
use crate::types::Episode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpisodePhase {
    Clean,
    Experimenting,
    CommitPending,
    Committed,
    Frozen,
}

pub struct EpisodeState {
    pub episode: Episode,
    pub phase: EpisodePhase,
    pub commit_request: Option<EvaluationPlan>,
    pub evaluation_decision: Option<EvaluationDecision>,
    pub rollback_required: bool,
    pub rollback_attempted: bool,
    pub rollback_succeeded: Option<bool>,
    pub rollback_error: Option<String>,
}

impl EpisodeState {
    pub fn new(episode: Episode) -> Self {
        Self {
            episode,
            phase: EpisodePhase::Clean,
            commit_request: None,
            evaluation_decision: None,
            rollback_required: false,
            rollback_attempted: false,
            rollback_succeeded: None,
            rollback_error: None,
        }
    }

    pub fn set_phase(&mut self, phase: EpisodePhase) {
        self.phase = phase;
    }
}
