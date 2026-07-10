use std::time::{Duration, Instant};

use crate::activation::{ActivationEvent, Severity};
use crate::types::AgentState;

pub struct ActivationKernel {
    state: AgentState,
    last_event_type: Option<String>,
    last_wake: Option<Instant>,
    dedupe_window: Duration,
}

impl Default for ActivationKernel {
    fn default() -> Self {
        Self {
            state: AgentState::Sleeping,
            last_event_type: None,
            last_wake: None,
            dedupe_window: Duration::from_secs(5),
        }
    }
}

impl ActivationKernel {
    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn accept(&mut self, event: &ActivationEvent) -> bool {
        if self.state == AgentState::Frozen {
            return false;
        }

        let now = Instant::now();
        let duplicate = self
            .last_wake
            .zip(self.last_event_type.as_ref())
            .map(|(last_wake, last_type)| {
                last_type == &event.event_type && now.duration_since(last_wake) < self.dedupe_window
            })
            .unwrap_or(false);

        if duplicate && event.severity < Severity::Critical {
            return false;
        }

        self.state = AgentState::Active;
        self.last_event_type = Some(event.event_type.clone());
        self.last_wake = Some(now);
        true
    }

    pub fn cooldown(&mut self, _duration: Duration) {
        self.state = AgentState::Cooldown;
    }

    pub fn sleep(&mut self) {
        self.state = AgentState::Sleeping;
    }
}
