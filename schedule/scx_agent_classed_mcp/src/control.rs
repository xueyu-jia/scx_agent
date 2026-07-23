use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::control_wire::{
    ControlOp, ControlRequest, ControlResponse, ControlStats, ControlStatus, RuleObservation,
    RuleState, CONTROL_VERSION,
};

use super::validate_comm;

pub trait SchedulerControl {
    fn get_rule(&self, request_id: String, comm: String) -> Result<ControlResponse>;
    fn snapshot(&self, request_id: String, comms: Vec<String>) -> Result<ControlResponse>;
    fn compare_and_set(
        &self,
        request_id: String,
        comm: String,
        expected: RuleState,
        desired: RuleState,
    ) -> Result<ControlResponse>;
}

#[derive(Clone)]
pub struct ControlClient {
    path: PathBuf,
    timeout: Duration,
}

impl ControlClient {
    pub fn new(path: PathBuf, timeout: Duration) -> Self {
        Self { path, timeout }
    }

    fn exchange(&self, request: ControlRequest) -> Result<ControlResponse> {
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
            .and_then(|socket| {
                socket.connect_timeout(&SockAddr::unix(&self.path)?, self.timeout)?;
                Ok(socket)
            })
            .with_context(|| {
                format!(
                    "failed to connect to control socket '{}'",
                    self.path.display()
                )
            })?;
        let mut stream = UnixStream::from(OwnedFd::from(socket));
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        stream.shutdown(Shutdown::Write)?;

        let mut data = Vec::new();
        stream
            .take(crate::control_wire::MAX_CONTROL_FRAME_BYTES + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > crate::control_wire::MAX_CONTROL_FRAME_BYTES {
            bail!(
                "control response exceeds {} bytes",
                crate::control_wire::MAX_CONTROL_FRAME_BYTES
            );
        }
        if data.is_empty() {
            bail!("control socket closed without a response");
        }
        let response: ControlResponse =
            serde_json::from_slice(&data).context("control socket returned an invalid response")?;
        if response.version != CONTROL_VERSION {
            bail!("unsupported control response version {}", response.version);
        }
        if response.request_id != request.request_id {
            bail!("control response request_id does not match request");
        }
        if response.effective_digest.is_empty() {
            bail!("control response has an empty effective_digest");
        }
        if response
            .current
            .as_ref()
            .is_some_and(|state| !state.is_valid())
        {
            bail!("control response contains an invalid current rule state");
        }
        if response.status == ControlStatus::Error {
            bail!(failure(&response));
        }
        Ok(response)
    }
}

impl SchedulerControl for ControlClient {
    fn get_rule(&self, request_id: String, comm: String) -> Result<ControlResponse> {
        let response = self.exchange(get_rule_request(request_id, comm.clone()))?;
        if response.status != ControlStatus::Ok {
            bail!(failure(&response));
        }
        validate_rule_set(&response, &[comm])?;
        Ok(response)
    }

    fn snapshot(&self, request_id: String, comms: Vec<String>) -> Result<ControlResponse> {
        let response = self.exchange(snapshot_request(request_id, comms.clone()))?;
        if response.status != ControlStatus::Ok {
            bail!(failure(&response));
        }
        validate_rule_set(&response, &comms)?;
        Ok(response)
    }

    fn compare_and_set(
        &self,
        request_id: String,
        comm: String,
        expected: RuleState,
        desired: RuleState,
    ) -> Result<ControlResponse> {
        self.exchange(compare_and_set_request(request_id, comm, expected, desired))
    }
}

pub fn require_current(response: &ControlResponse) -> Result<RuleState> {
    response
        .current
        .clone()
        .ok_or_else(|| anyhow!("control response is missing current rule state"))
        .and_then(|state| {
            if state.is_valid() {
                Ok(state)
            } else {
                bail!("control response current rule state is invalid")
            }
        })
}

pub fn require_rule<'a>(response: &'a ControlResponse, comm: &str) -> Result<&'a RuleObservation> {
    let mut matches = response.rules.iter().filter(|rule| rule.comm == comm);
    let rule = matches
        .next()
        .ok_or_else(|| anyhow!("control response is missing rule '{comm}'"))?;
    if matches.next().is_some() {
        bail!("control response contains duplicate rule '{comm}'");
    }
    validate_comm(&rule.comm)?;
    Ok(rule)
}

pub fn require_stats(response: &ControlResponse) -> Result<&ControlStats> {
    response
        .stats
        .as_ref()
        .ok_or_else(|| anyhow!("control response is missing scheduler integrity stats"))
}

pub fn failure(response: &ControlResponse) -> String {
    response
        .message
        .clone()
        .unwrap_or_else(|| format!("control operation returned status {:?}", response.status))
}

fn validate_rule_set(response: &ControlResponse, expected: &[String]) -> Result<()> {
    if response.rules.len() != expected.len() {
        bail!(
            "control response returned {} rules for {} requested comms",
            response.rules.len(),
            expected.len()
        );
    }
    let actual = response
        .rules
        .iter()
        .map(|rule| {
            validate_comm(&rule.comm)?;
            Ok(rule.comm.as_str())
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let requested = expected.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if actual.len() != response.rules.len() || actual != requested {
        bail!("control response rule set does not match the request");
    }
    Ok(())
}

fn get_rule_request(request_id: String, comm: String) -> ControlRequest {
    ControlRequest {
        version: CONTROL_VERSION,
        request_id,
        op: ControlOp::GetRule,
        comm: Some(comm),
        comms: None,
        expected: None,
        desired: None,
    }
}

fn snapshot_request(request_id: String, comms: Vec<String>) -> ControlRequest {
    ControlRequest {
        version: CONTROL_VERSION,
        request_id,
        op: ControlOp::Snapshot,
        comm: None,
        comms: Some(comms),
        expected: None,
        desired: None,
    }
}

fn compare_and_set_request(
    request_id: String,
    comm: String,
    expected: RuleState,
    desired: RuleState,
) -> ControlRequest {
    ControlRequest {
        version: CONTROL_VERSION,
        request_id,
        op: ControlOp::CompareAndSetRule,
        comm: Some(comm),
        comms: None,
        expected: Some(expected),
        desired: Some(desired),
    }
}
