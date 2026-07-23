use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam::channel::{self, Receiver, Sender, TrySendError};
use serde::Serialize;
use socket2::{Domain, SockAddr, Socket, Type};

const IO_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct RuleMissActivation {
    pub comms: Vec<String>,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotifyOutcome {
    Queued,
    Suppressed,
    Full,
    Disconnected,
}

pub struct ActivationNotifier {
    sender: Option<Sender<RuleMissActivation>>,
    claimed_comms: Arc<Mutex<BTreeSet<String>>>,
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl ActivationNotifier {
    pub fn start(socket_path: PathBuf, scheduler_instance_id: String) -> Self {
        let (sender, receiver) = channel::bounded::<RuleMissActivation>(64);
        let (stop, stopped) = channel::bounded::<()>(1);
        let claimed_comms = Arc::new(Mutex::new(BTreeSet::new()));
        let worker_claimed_comms = Arc::clone(&claimed_comms);
        let worker = std::thread::spawn(move || loop {
            if stopped.try_recv().is_ok() {
                break;
            }
            crossbeam::select! {
                recv(stopped) -> _ => break,
                recv(receiver) -> activation => match activation {
                    Ok(activation) => {
                        let comms = activation.comms.clone();
                        if let Err(error) = send_activation(
                            &socket_path,
                            &scheduler_instance_id,
                            activation,
                            &stopped,
                        ) {
                            log::warn!("failed to notify tuning-agent: {error}");
                        }
                        release_claims(&worker_claimed_comms, &comms);
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            sender: Some(sender),
            claimed_comms,
            stop,
            worker: Some(worker),
        }
    }

    pub fn notify(&self, mut activation: RuleMissActivation) -> NotifyOutcome {
        let Some(sender) = &self.sender else {
            log::warn!("dropping tuning-agent activation: notifier is disconnected");
            return NotifyOutcome::Disconnected;
        };

        if claim_unseen(&self.claimed_comms, &mut activation.comms).is_err() {
            log::warn!("dropping tuning-agent activation: comm claim lock is poisoned");
            return NotifyOutcome::Disconnected;
        }
        if activation.comms.is_empty() {
            return NotifyOutcome::Suppressed;
        }

        match sender.try_send(activation) {
            Ok(()) => NotifyOutcome::Queued,
            Err(TrySendError::Full(activation)) => {
                release_claims(&self.claimed_comms, &activation.comms);
                log::warn!("delaying tuning-agent activation: notifier queue is full");
                NotifyOutcome::Full
            }
            Err(TrySendError::Disconnected(activation)) => {
                release_claims(&self.claimed_comms, &activation.comms);
                log::warn!("dropping tuning-agent activation: notifier is disconnected");
                NotifyOutcome::Disconnected
            }
        }
    }
}

impl Drop for ActivationNotifier {
    fn drop(&mut self) {
        self.sender.take();
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Serialize)]
struct ActivationRequest<'a> {
    request_id: String,
    wait: bool,
    event: ActivationEvent<'a>,
}

#[derive(Serialize)]
struct ActivationEvent<'a> {
    source: ProgramSource<'a>,
    event_type: &'static str,
    severity: &'static str,
    scope: &'static str,
    timestamp_ns: u128,
    evidence: ActivationEvidence<'a>,
}

#[derive(Serialize)]
enum ProgramSource<'a> {
    Program(&'a str),
}

#[derive(Serialize)]
struct ActivationEvidence<'a> {
    scheduler_instance_id: &'a str,
    unknown_comms: &'a [String],
    persistent_revision: u64,
}

fn send_activation(
    socket_path: &PathBuf,
    scheduler_instance_id: &str,
    activation: RuleMissActivation,
    stop: &Receiver<()>,
) -> std::io::Result<()> {
    let timestamp_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let request_id = format!("scx-agent-classed-{scheduler_instance_id}-{timestamp_ns}");
    let payload = encode_activation(
        scheduler_instance_id,
        &activation,
        timestamp_ns,
        &request_id,
    )?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(socket_path)?, IO_TIMEOUT)?;
    let mut stream = UnixStream::from(OwnedFd::from(socket));
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if stop.try_recv().is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "activation notifier is stopping",
            ));
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "tuning-agent activation response exceeds 64 KiB",
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }

    let value: serde_json::Value = serde_json::from_slice(&response)?;
    if value.get("request_id").and_then(serde_json::Value::as_str) != Some(&request_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tuning-agent activation response has the wrong request_id",
        ));
    }
    if value
        .get("accepted")
        .and_then(serde_json::Value::as_bool)
        .is_none()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tuning-agent activation response has no accepted flag",
        ));
    }
    Ok(())
}

fn encode_activation(
    scheduler_instance_id: &str,
    activation: &RuleMissActivation,
    timestamp_ns: u128,
    request_id: &str,
) -> std::io::Result<Vec<u8>> {
    let request = ActivationRequest {
        request_id: request_id.to_string(),
        wait: true,
        event: ActivationEvent {
            source: ProgramSource::Program("scx_agent_classed"),
            event_type: "scheduler.rule_miss.v1",
            severity: "Info",
            scope: "Host",
            timestamp_ns,
            evidence: ActivationEvidence {
                scheduler_instance_id,
                unknown_comms: &activation.comms,
                persistent_revision: activation.revision,
            },
        },
    };
    serde_json::to_vec(&request).map_err(std::io::Error::other)
}

fn release_claims(claimed_comms: &Mutex<BTreeSet<String>>, comms: &[String]) {
    if let Ok(mut claimed) = claimed_comms.lock() {
        for comm in comms {
            claimed.remove(comm);
        }
    }
}

fn claim_unseen(
    claimed_comms: &Mutex<BTreeSet<String>>,
    comms: &mut Vec<String>,
) -> Result<(), ()> {
    let mut claimed = claimed_comms.lock().map_err(|_| ())?;
    comms.retain(|comm| claimed.insert(comm.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_matches_tuning_agent_wire_shape() {
        let payload = encode_activation(
            "instance-1",
            &RuleMissActivation {
                comms: vec!["worker".to_string()],
                revision: 7,
            },
            42,
            "request-1",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        assert_eq!(value["request_id"], "request-1");
        assert_eq!(value["wait"], true);
        assert_eq!(value["event"]["source"]["Program"], "scx_agent_classed");
        assert_eq!(value["event"]["severity"], "Info");
        assert_eq!(value["event"]["scope"], "Host");
        assert_eq!(value["event"]["evidence"]["unknown_comms"][0], "worker");
        assert_eq!(value["event"]["evidence"]["persistent_revision"], 7);
    }

    #[test]
    fn comm_claims_suppress_duplicates_until_release() {
        let claims = Mutex::new(BTreeSet::new());

        let mut first = vec!["worker".to_string(), "service".to_string()];
        claim_unseen(&claims, &mut first).unwrap();
        assert_eq!(first, ["worker", "service"]);

        let mut duplicate = vec!["worker".to_string()];
        claim_unseen(&claims, &mut duplicate).unwrap();
        assert!(duplicate.is_empty());

        release_claims(&claims, &["worker".to_string()]);
        let mut retry = vec!["worker".to_string()];
        claim_unseen(&claims, &mut retry).unwrap();
        assert_eq!(retry, ["worker"]);
    }
}
