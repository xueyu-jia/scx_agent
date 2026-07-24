use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam::channel::{self, Receiver, Sender, TryRecvError, TrySendError};
use serde::{Deserialize, Serialize};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::rules::Comm;

const IO_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct RuleMissActivation {
    pub comms: Vec<String>,
    pub revision: u64,
}

#[derive(Debug)]
pub struct ActivationBatcher {
    pending: BTreeSet<Comm>,
    pending_since: Option<Instant>,
    in_flight: Option<Vec<Comm>>,
    coalesce: Duration,
    max_batch: usize,
}

impl ActivationBatcher {
    pub fn new(coalesce: Duration, max_batch: usize) -> Self {
        assert!(max_batch > 0);
        Self {
            pending: BTreeSet::new(),
            pending_since: None,
            in_flight: None,
            coalesce,
            max_batch,
        }
    }

    pub fn observe(&mut self, comm: Comm, now: Instant) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|batch| batch.contains(&comm))
        {
            return;
        }
        if self.pending.insert(comm) && self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
    }

    pub fn remove_pending(&mut self, comm: &Comm) {
        self.pending.remove(comm);
        if self.pending.is_empty() {
            self.pending_since = None;
        }
    }

    pub fn pending(&self) -> impl Iterator<Item = &Comm> {
        self.pending.iter()
    }

    pub fn take_ready(&mut self, now: Instant) -> Option<Vec<Comm>> {
        if self.in_flight.is_some() || self.pending.is_empty() {
            return None;
        }
        let ready = self.pending.len() >= self.max_batch
            || self
                .pending_since
                .is_some_and(|started| now.duration_since(started) >= self.coalesce);
        if !ready {
            return None;
        }

        let batch = self
            .pending
            .iter()
            .take(self.max_batch)
            .cloned()
            .collect::<Vec<_>>();
        for comm in &batch {
            self.pending.remove(comm);
        }
        if self.pending.is_empty() {
            self.pending_since = None;
        }
        self.in_flight = Some(batch.clone());
        Some(batch)
    }

    pub fn finish(&mut self) -> Option<Vec<Comm>> {
        self.in_flight.take()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationResponse {
    pub accepted: bool,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationCompletion {
    Response(ActivationResponse),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotifierError {
    Busy,
    Disconnected,
}

pub struct ActivationNotifier {
    commands: Option<Sender<RuleMissActivation>>,
    completions: Receiver<ActivationCompletion>,
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl ActivationNotifier {
    pub fn start(socket_path: PathBuf, scheduler_instance_id: String) -> Self {
        let (commands, command_receiver) = channel::bounded::<RuleMissActivation>(1);
        let (completion_sender, completions) = channel::bounded::<ActivationCompletion>(1);
        let (stop, stopped) = channel::bounded::<()>(1);
        let worker = std::thread::spawn(move || loop {
            let activation = crossbeam::select! {
                recv(stopped) -> _ => break,
                recv(command_receiver) -> command => match command {
                    Ok(command) => command,
                    Err(_) => break,
                },
            };
            let completion =
                match send_activation(&socket_path, &scheduler_instance_id, activation, &stopped) {
                    Ok(response) => ActivationCompletion::Response(response),
                    Err(error) => ActivationCompletion::Failed(error.to_string()),
                };
            crossbeam::select! {
                recv(stopped) -> _ => break,
                send(completion_sender, completion) -> result => {
                    if result.is_err() {
                        break;
                    }
                },
            }
        });
        Self {
            commands: Some(commands),
            completions,
            stop,
            worker: Some(worker),
        }
    }

    pub fn start_activation(&self, activation: RuleMissActivation) -> Result<(), NotifierError> {
        let Some(commands) = &self.commands else {
            return Err(NotifierError::Disconnected);
        };
        match commands.try_send(activation) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(NotifierError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(NotifierError::Disconnected),
        }
    }

    pub fn poll_completion(&self) -> Result<Option<ActivationCompletion>, NotifierError> {
        match self.completions.try_recv() {
            Ok(completion) => Ok(Some(completion)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(NotifierError::Disconnected),
        }
    }
}

impl Drop for ActivationNotifier {
    fn drop(&mut self) {
        self.commands.take();
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

#[derive(Deserialize)]
struct WireActivationResponse {
    request_id: String,
    status: String,
    accepted: bool,
    error: Option<String>,
}

fn send_activation(
    socket_path: &PathBuf,
    scheduler_instance_id: &str,
    activation: RuleMissActivation,
    stop: &Receiver<()>,
) -> std::io::Result<ActivationResponse> {
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

    let response: WireActivationResponse =
        serde_json::from_slice(&response).map_err(std::io::Error::other)?;
    if response.request_id != request_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tuning-agent activation response has the wrong request_id",
        ));
    }
    Ok(ActivationResponse {
        accepted: response.accepted,
        status: response.status,
        error: response.error,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn comm(value: &str) -> Comm {
        Comm::new(value).unwrap()
    }

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
    fn batcher_waits_for_the_fixed_coalescing_window() {
        let start = Instant::now();
        let mut batcher = ActivationBatcher::new(Duration::from_millis(250), 128);
        batcher.observe(comm("worker-b"), start);
        batcher.observe(comm("worker-a"), start + Duration::from_millis(100));

        assert!(batcher
            .take_ready(start + Duration::from_millis(249))
            .is_none());
        assert_eq!(
            batcher.take_ready(start + Duration::from_millis(250)),
            Some(vec![comm("worker-a"), comm("worker-b")])
        );
    }

    #[test]
    fn batcher_accumulates_one_next_batch_while_busy() {
        let start = Instant::now();
        let mut batcher = ActivationBatcher::new(Duration::from_millis(250), 128);
        batcher.observe(comm("first"), start);
        assert_eq!(
            batcher.take_ready(start + Duration::from_millis(250)),
            Some(vec![comm("first")])
        );

        batcher.observe(comm("third"), start + Duration::from_millis(300));
        batcher.observe(comm("second"), start + Duration::from_millis(350));
        assert!(batcher
            .take_ready(start + Duration::from_millis(600))
            .is_none());
        assert_eq!(batcher.finish(), Some(vec![comm("first")]));
        assert_eq!(
            batcher.take_ready(start + Duration::from_millis(600)),
            Some(vec![comm("second"), comm("third")])
        );
    }

    #[test]
    fn batcher_sends_at_capacity_and_preserves_the_remainder() {
        let start = Instant::now();
        let mut batcher = ActivationBatcher::new(Duration::from_secs(60), 2);
        for value in ["worker-c", "worker-a", "worker-b"] {
            batcher.observe(comm(value), start);
        }

        assert_eq!(
            batcher.take_ready(start),
            Some(vec![comm("worker-a"), comm("worker-b")])
        );
        assert_eq!(batcher.finish().unwrap().len(), 2);
        assert_eq!(
            batcher.take_ready(start + Duration::from_secs(60)),
            Some(vec![comm("worker-c")])
        );
    }

    #[test]
    fn batcher_suppresses_pending_and_in_flight_duplicates() {
        let start = Instant::now();
        let mut batcher = ActivationBatcher::new(Duration::ZERO, 128);
        batcher.observe(comm("worker"), start);
        batcher.observe(comm("worker"), start);
        assert_eq!(batcher.pending().count(), 1);
        assert_eq!(batcher.take_ready(start), Some(vec![comm("worker")]));

        batcher.observe(comm("worker"), start);
        assert_eq!(batcher.pending().count(), 0);
        assert_eq!(batcher.finish(), Some(vec![comm("worker")]));
    }
}
