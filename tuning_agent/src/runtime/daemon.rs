use std::thread;
use std::time::Duration;

use crate::activation::source::{EbpfRingbufSource, TimerSource, UnixIpcSource};
use crate::activation::{ActivationEvent, ActivationKernel};
use crate::audit::AuditJournal;
use crate::config::Config;
use crate::runtime::episode_controller::{EpisodeController, EpisodeOutcome};
use crate::runtime::episode_state::EpisodePhase;
use crate::types::Episode;

pub struct Runtime {
    config: Config,
    activation: ActivationKernel,
    audit: AuditJournal,
}

impl Runtime {
    pub fn new(config: Config) -> Self {
        let audit = AuditJournal::new(config.audit.path.clone());
        Self {
            config,
            activation: ActivationKernel::default(),
            audit,
        }
    }

    pub fn run_daemon(&mut self) -> std::io::Result<()> {
        let mut unix_source = UnixIpcSource::bind(self.config.activation.socket_path.clone())?;
        let mut timer_source = TimerSource::new(self.config.activation.timer_interval_ms);
        let mut ebpf_source =
            EbpfRingbufSource::new(self.config.activation.ebpf_ringbuf_pin.clone());

        println!(
            "tuning-agent daemon listening on {}",
            unix_source.path().display()
        );

        loop {
            let mut events = unix_source.poll()?;
            events.extend(timer_source.poll());
            events.extend(ebpf_source.poll());

            for event in events {
                self.process_activation_event(event)?;
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    fn process_activation_event(&mut self, event: ActivationEvent) -> std::io::Result<()> {
        if !self.activation.accept(&event) {
            self.audit
                .record_activation_rejected(&event, self.activation.state())?;
            return Ok(());
        }

        let episode = Episode::new(event);
        let state = self.activation.state();
        let EpisodeOutcome {
            episode,
            phase,
            act_result,
        } = {
            let mut controller = EpisodeController::new(self.config.clone(), &mut self.audit);
            controller.run(episode, state)?
        };

        if phase == EpisodePhase::Frozen {
            self.activation.freeze();
        } else {
            self.activation.cooldown(Duration::from_secs(30));
            self.activation.sleep();
        }
        self.audit.record_episode_finished(
            &episode,
            self.activation.state(),
            phase,
            &act_result,
        )?;

        Ok(())
    }
}
