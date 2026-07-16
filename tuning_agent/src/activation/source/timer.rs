use std::time::{Duration, Instant};

use crate::activation::Scope;
use crate::activation::{ActivationEvent, EventSource, Severity};

pub struct TimerSource {
    interval: Option<Duration>,
    next_tick: Instant,
}

impl TimerSource {
    pub fn new(interval_ms: Option<u64>) -> Self {
        let interval = interval_ms
            .filter(|value| *value > 0)
            .map(Duration::from_millis);
        Self {
            interval,
            next_tick: Instant::now(),
        }
    }

    pub fn poll(&mut self) -> Vec<ActivationEvent> {
        let Some(interval) = self.interval else {
            return Vec::new();
        };

        let now = Instant::now();
        if now < self.next_tick {
            return Vec::new();
        }

        self.next_tick = now + interval;
        vec![ActivationEvent::new(
            EventSource::Internal,
            "timer".to_string(),
            Severity::Info,
            Scope::Host,
        )]
    }
}
