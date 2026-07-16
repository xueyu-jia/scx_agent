use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::activation::source::{TimerSource, UnixActivation, UnixIpcSource};
use crate::activation::{ActivationKernel, ActivationRequest, ActivationResponse};
use crate::adapters::openai::OpenAiReasoner;
use crate::audit::{AuditRecord, AuditSink, JsonlAuditSink};
use crate::capability::CapabilityRegistry;
use crate::config::Config;
use crate::domain::{EpisodeId, EpisodePhase};
use crate::kernel::transaction::TransactionStore;
use crate::runtime::bootstrap::{
    build_local_registry, extend_registry_with_mcp, RegistryBootstrap,
};
use crate::runtime::episode::EpisodeCoordinator;
use crate::runtime::recovery::{
    recover_available_before_plugin_bootstrap, recover_before_activation,
};

pub struct Runtime {
    config: Config,
    activation: ActivationKernel,
    audit: JsonlAuditSink,
    capabilities: CapabilityRegistry,
    transactions: TransactionStore,
}

impl Runtime {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        // Store construction acquires the process-level WAL directory lock.
        // It must precede plugin startup, audit writes, and recovery discovery.
        let transactions = TransactionStore::new(&config.transaction.wal_dir)
            .map_err(|error| error.to_string())?;
        let mut bootstrap = build_local_registry(&config.capabilities, &config.mcp);
        recover_available_before_plugin_bootstrap(&transactions, bootstrap.registry.snapshot());
        extend_registry_with_mcp(&mut bootstrap, &config.mcp);
        let RegistryBootstrap {
            registry: capabilities,
            notices: bootstrap_notices,
            failures: bootstrap_failures,
        } = bootstrap;
        let mut audit = JsonlAuditSink::new(&config.audit.path);
        let mut activation_blockers = Vec::new();

        if let Err(error) =
            recover_before_activation(&transactions, capabilities.snapshot(), &mut audit)
        {
            activation_blockers.push(error);
        }

        for notice in bootstrap_notices {
            if let Err(error) = audit.record(&AuditRecord::runtime("mcp_server_loaded", notice)) {
                activation_blockers.push(format!("failed to audit MCP bootstrap: {error}"));
            }
        }
        for failure in bootstrap_failures {
            activation_blockers.push(format!(
                "capability bootstrap '{}' failed: {}",
                failure.component, failure.error
            ));
            if let Err(error) = audit.record(&AuditRecord::runtime(
                "capability_bootstrap_failed",
                json!({
                    "component": failure.component,
                    "error": failure.error,
                }),
            )) {
                activation_blockers.push(format!("failed to audit bootstrap failure: {error}"));
            }
        }
        if let Err(error) = audit.record(&AuditRecord::runtime(
            "capability_registry_ready",
            json!({
                "generation": capabilities.snapshot().generation(),
                "capability_count": capabilities.len(),
            }),
        )) {
            activation_blockers.push(format!("failed to initialize audit sink: {error}"));
        }

        if !activation_blockers.is_empty() {
            return Err(format!(
                "runtime activation blocked after recovery: {}",
                activation_blockers.join("; ")
            ));
        }
        Ok(Self {
            config,
            activation: ActivationKernel::default(),
            audit,
            capabilities,
            transactions,
        })
    }

    pub fn run_daemon(&mut self) -> std::io::Result<()> {
        let mut unix_source = UnixIpcSource::bind(self.config.activation.socket_path.clone())?;
        let mut timer_source = TimerSource::new(self.config.activation.timer_interval_ms);

        println!(
            "tuning-agent daemon listening on {}",
            unix_source.path().display()
        );
        loop {
            for activation in unix_source.poll()? {
                self.process_unix_activation(activation)?;
            }
            for event in timer_source.poll() {
                let request = ActivationRequest::fire_and_forget(event);
                let _ = self.process_activation_request(request)?;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn process_unix_activation(&mut self, activation: UnixActivation) -> std::io::Result<()> {
        let wants_response = activation.wants_response();
        let request = activation.request.clone();
        let response = self.process_activation_request(request)?;
        if wants_response {
            activation.respond(&response)?;
        }
        Ok(())
    }

    fn process_activation_request(
        &mut self,
        request: ActivationRequest,
    ) -> std::io::Result<ActivationResponse> {
        let event = request.event.clone();
        if !self.activation.accept(&event) {
            self.audit.record(&AuditRecord::runtime(
                "activation_rejected",
                json!({
                    "event": event,
                    "activation_state": format!("{:?}", self.activation.state()),
                    "request_id": request.request_id,
                }),
            ))?;
            return Ok(ActivationResponse::rejected(
                request.request_id,
                format!(
                    "activation rejected while kernel was {:?}",
                    self.activation.state()
                ),
            ));
        }

        let episode_id = next_episode_id();
        let activation = serde_json::to_value(&event).map_err(std::io::Error::other)?;
        let mut reasoner = match OpenAiReasoner::new(&self.config.llm) {
            Ok(reasoner) => reasoner,
            Err(error) => {
                self.activation.sleep();
                self.audit.record(&AuditRecord::episode(
                    "episode_rejected",
                    episode_id,
                    EpisodePhase::Clean,
                    json!({
                        "error": error,
                        "request_id": request.request_id,
                    }),
                ))?;
                return Ok(ActivationResponse::error(request.request_id, error));
            }
        };
        let outcome = match EpisodeCoordinator::new(
            self.config.reasoning.max_rounds,
            Duration::from_millis(self.config.safety.evaluation_timeout_ms),
            self.capabilities.snapshot(),
            &self.transactions,
            &mut self.audit,
        )
        .and_then(|mut coordinator| coordinator.run(episode_id, activation, &mut reasoner))
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.activation.sleep();
                return Ok(ActivationResponse::error(
                    request.request_id,
                    error.to_string(),
                ));
            }
        };

        if outcome.phase == EpisodePhase::RecoveryRequired {
            self.activation.freeze();
        } else {
            self.activation
                .cooldown(Duration::from_millis(self.config.safety.cooldown_ms));
        }
        println!(
            "episode {} finished; phase={:?}; {}",
            outcome.episode_id, outcome.phase, outcome.summary
        );
        Ok(ActivationResponse::from_episode(
            request.request_id,
            outcome,
        ))
    }
}

fn next_episode_id() -> EpisodeId {
    static LAST_ID: AtomicU64 = AtomicU64::new(0);
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    let mut observed = LAST_ID.load(Ordering::Relaxed);
    loop {
        let next = clock.max(observed.saturating_add(1));
        match LAST_ID.compare_exchange_weak(observed, next, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return EpisodeId::new(next),
            Err(current) => observed = current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::transaction::{TransactionWal, WalEntry, WalEvent};

    #[test]
    fn generated_episode_ids_are_monotonic() {
        assert!(next_episode_id().get() < next_episode_id().get());
    }

    #[test]
    fn a_second_runtime_cannot_share_the_transaction_directory() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-runtime-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = Config::default();
        config.transaction.wal_dir = root.join("transactions");
        config.audit.path = root.join("audit.jsonl");
        let first = Runtime::new(config.clone()).unwrap();
        config.mcp.servers.push(crate::config::McpServerConfig {
            id: "must-not-start".into(),
            command: "/definitely/missing/mcp-server".into(),
            request_timeout_ms: 1,
            ..crate::config::McpServerConfig::default()
        });

        let error = Runtime::new(config)
            .err()
            .expect("second runtime must fail");

        assert!(error.contains("already owned by another runtime"));
        drop(first);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mcp_bootstrap_failure_does_not_prevent_early_transaction_recovery() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-runtime-recovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = Config::default();
        config.transaction.wal_dir = root.join("transactions");
        config.audit.path = root.join("audit.jsonl");
        config.mcp.servers.push(crate::config::McpServerConfig {
            id: "unavailable".into(),
            command: "/definitely/missing/mcp-server".into(),
            request_timeout_ms: 1,
            ..crate::config::McpServerConfig::default()
        });
        let transaction_id = crate::domain::TransactionId::new("pending").unwrap();
        let store = TransactionStore::new(&config.transaction.wal_dir).unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        wal.append_durable(&WalEntry {
            sequence: 0,
            transaction_id: transaction_id.clone(),
            event: WalEvent::Started {
                intent_pin: crate::domain::EvaluationIntentPin::new(
                    EpisodeId::new(1),
                    crate::domain::Digest::new("test-intent").unwrap(),
                    crate::domain::Digest::new("test-contract").unwrap(),
                ),
                capability_generation: 0,
            },
        })
        .unwrap();
        drop(wal);
        drop(store);

        let error = Runtime::new(config.clone()).err().unwrap();

        assert!(error.contains("mcp_server/unavailable"));
        let store = TransactionStore::new(&config.transaction.wal_dir).unwrap();
        let inventory = store.discover().unwrap();
        assert!(inventory.pending.is_empty());
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, transaction_id);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
