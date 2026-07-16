use std::time::{Duration, Instant};

use crate::activation::{ActivationEvent, Severity};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationState {
    Sleeping,
    Active,
    Cooldown,
    Frozen,
}

pub struct ActivationKernel {
    state: ActivationState,
    cooldown_until: Option<Instant>,
    last_event_type: Option<String>,
    last_wake: Option<Instant>,
    dedupe_window: Duration,
}

impl Default for ActivationKernel {
    fn default() -> Self {
        Self {
            state: ActivationState::Sleeping,
            cooldown_until: None,
            last_event_type: None,
            last_wake: None,
            dedupe_window: Duration::from_secs(5),
        }
    }
}

impl ActivationKernel {
    pub fn state(&self) -> ActivationState {
        self.state
    }

    pub fn accept(&mut self, event: &ActivationEvent) -> bool {
        self.accept_at(event, Instant::now())
    }

    fn accept_at(&mut self, event: &ActivationEvent, now: Instant) -> bool {
        if self.state == ActivationState::Frozen {
            return false;
        }

        if self.state == ActivationState::Cooldown {
            if self
                .cooldown_until
                .is_none_or(|cooldown_until| now < cooldown_until)
            {
                return false;
            }
            self.state = ActivationState::Sleeping;
            self.cooldown_until = None;
        }

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

        self.state = ActivationState::Active;
        self.cooldown_until = None;
        self.last_event_type = Some(event.event_type.clone());
        self.last_wake = Some(now);
        true
    }

    pub fn cooldown(&mut self, duration: Duration) {
        self.cooldown_at(duration, Instant::now());
    }

    fn cooldown_at(&mut self, duration: Duration, now: Instant) {
        self.state = ActivationState::Cooldown;
        // Overflow is treated as an indefinite cooldown, which fails closed.
        self.cooldown_until = now.checked_add(duration);
    }

    pub fn freeze(&mut self) {
        self.state = ActivationState::Frozen;
        self.cooldown_until = None;
    }

    pub fn sleep(&mut self) {
        self.state = ActivationState::Sleeping;
        self.cooldown_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activation::EventSource;
    use crate::activation::Scope;

    #[test]
    fn frozen_kernel_rejects_new_activations() {
        let mut kernel = ActivationKernel::default();
        let first = ActivationEvent::new(
            EventSource::Cli,
            "first".to_string(),
            Severity::Info,
            Scope::Host,
        );
        assert!(kernel.accept(&first));

        kernel.freeze();

        let critical = ActivationEvent::new(
            EventSource::Cli,
            "critical".to_string(),
            Severity::Critical,
            Scope::Host,
        );
        assert_eq!(kernel.state(), ActivationState::Frozen);
        assert!(!kernel.accept(&critical));
    }

    #[test]
    fn cooldown_rejects_all_events_until_its_deadline() {
        let mut kernel = ActivationKernel::default();
        let start = Instant::now();
        let first = ActivationEvent::new(
            EventSource::Cli,
            "first".to_string(),
            Severity::Info,
            Scope::Host,
        );
        assert!(kernel.accept_at(&first, start));

        kernel.cooldown_at(Duration::from_secs(30), start);
        let critical = ActivationEvent::new(
            EventSource::Cli,
            "critical".to_string(),
            Severity::Critical,
            Scope::Host,
        );
        assert_eq!(kernel.state(), ActivationState::Cooldown);
        assert!(!kernel.accept_at(&critical, start + Duration::from_secs(29)));
        assert!(kernel.accept_at(&critical, start + Duration::from_secs(30)));
        assert_eq!(kernel.state(), ActivationState::Active);
    }
}
