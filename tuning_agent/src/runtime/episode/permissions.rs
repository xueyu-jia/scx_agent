use crate::domain::EpisodePhase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentAction {
    Probe,
    BeginExperiment,
    Mutation,
    RequestCommit,
    Abort,
}

impl AgentAction {
    pub fn is_allowed_in(
        self,
        phase: EpisodePhase,
        episode_active: bool,
        has_frozen_intent: bool,
    ) -> bool {
        if !episode_active {
            return false;
        }
        match phase {
            EpisodePhase::Clean => match self {
                Self::Probe => true,
                Self::BeginExperiment => !has_frozen_intent,
                Self::Mutation => has_frozen_intent,
                Self::Abort => true,
                Self::RequestCommit => false,
            },
            EpisodePhase::Experimenting => matches!(
                self,
                Self::Probe | Self::Mutation | Self::RequestCommit | Self::Abort
            ),
            EpisodePhase::CommitPending
            | EpisodePhase::RollingBack
            | EpisodePhase::RecoveryRequired
            | EpisodePhase::Committed => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_phase_freezes_only_one_intent() {
        assert!(AgentAction::BeginExperiment.is_allowed_in(EpisodePhase::Clean, true, false));
        assert!(!AgentAction::BeginExperiment.is_allowed_in(EpisodePhase::Clean, true, true));
        assert!(AgentAction::Mutation.is_allowed_in(EpisodePhase::Clean, true, true));
        assert!(AgentAction::Abort.is_allowed_in(EpisodePhase::Clean, true, false));
        assert!(!AgentAction::Probe.is_allowed_in(EpisodePhase::Clean, false, false));
    }
}
