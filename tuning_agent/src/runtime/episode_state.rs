use crate::evaluate::{EvaluationDecision, EvaluationPlan};
use crate::types::Episode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpisodePhase {
    Clean,
    Experimenting,
    CommitPending,
    Committed,
    Frozen,
    Finished,
}

pub struct EpisodeState {
    pub episode: Episode,
    pub phase: EpisodePhase,
    pub commit_request: Option<EvaluationPlan>,
    pub evaluation_decision: Option<EvaluationDecision>,
}

impl EpisodeState {
    pub fn new(episode: Episode) -> Self {
        Self {
            episode,
            phase: EpisodePhase::Clean,
            commit_request: None,
            evaluation_decision: None,
        }
    }

    pub fn set_phase(&mut self, phase: EpisodePhase) {
        self.phase = phase;
    }
}
