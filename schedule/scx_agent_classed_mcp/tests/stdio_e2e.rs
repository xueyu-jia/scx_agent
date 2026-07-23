#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scx_agent_classed_control::{
    ControlOp, ControlRequest, ControlResponse, ControlStats, ControlStatus, RuleClass,
    RuleObservation, RuleSource, RuleState, CONTROL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const COMM: &str = "e2e-worker";

#[test]
fn stdio_mcp_drives_a_strict_persisted_rule_lifecycle() {
    let root = TestDir::new();
    let scheduler = FakeScheduler::start(&root);
    let journal = root.path().join("mcp-journal.json");
    let mut mcp = McpProcess::start(scheduler.socket(), &journal);

    let initialized = mcp.rpc(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "scx-agent-classed-e2e", "version": "1"}
        }),
    );
    assert_eq!(initialized["protocolVersion"], "2024-11-05");

    let resource = mcp.rpc("resources/read", json!({"uri": "tuning://capabilities/v1"}));
    let manifest: Value = serde_json::from_str(
        resource["contents"][0]["text"]
            .as_str()
            .expect("capability resource text"),
    )
    .expect("valid capability manifest");
    let mutation = manifest["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|capability| capability["id"] == "rule.upsert.v1")
        .expect("rule.upsert.v1 capability");

    let listed = mcp.rpc("tools/list", json!({}));
    let tool_names = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    for phase in [
        "prepare", "apply", "status", "verify", "restore", "finalize",
    ] {
        assert!(tool_names.contains(operation(mutation, phase)));
    }

    let raw_prepared = mcp.call_tool(
        operation(mutation, "prepare"),
        json!({
            "context": {"episode_id": 7, "operation_id": "e2e/prepare"},
            "arguments": {"comm": COMM, "class": "latency"}
        }),
    );
    assert_eq!(raw_prepared["baseline"]["value"], json!({"present": false}));
    assert_eq!(
        raw_prepared["desired"]["value"],
        json!({"present": true, "class": "latency"})
    );
    let prepared = pin_prepared(&manifest, raw_prepared);

    let applied = mcp.call_tool(
        operation(mutation, "apply"),
        operation_request("e2e/apply", &prepared),
    );
    assert_eq!(applied["state"], "applied");

    let status = mcp.call_tool(
        operation(mutation, "status"),
        json!({"operation_id": "e2e/apply"}),
    );
    assert_eq!(status["state"], "applied");

    let verified = mcp.call_tool(
        operation(mutation, "verify"),
        verify_request("e2e/verify-desired", &prepared, &prepared["desired"]),
    );
    assert_eq!(verified["matched"], true);
    assert_eq!(verified["details"]["consistent"], true);

    let restored = mcp.call_tool(
        operation(mutation, "restore"),
        operation_request("e2e/restore", &prepared),
    );
    assert_eq!(restored["state"], "restored");
    let restore_status = mcp.call_tool(
        operation(mutation, "status"),
        json!({"operation_id": "e2e/restore"}),
    );
    assert_eq!(restore_status["state"], "restored");
    let baseline = mcp.call_tool(
        operation(mutation, "verify"),
        verify_request("e2e/verify-baseline", &prepared, &prepared["baseline"]),
    );
    assert_eq!(baseline["matched"], true);

    let reapplied = mcp.call_tool(
        operation(mutation, "apply"),
        operation_request("e2e/reapply", &prepared),
    );
    assert_eq!(reapplied["state"], "applied");
    let finalized = mcp.call_tool(
        operation(mutation, "finalize"),
        operation_request("e2e/finalize", &prepared),
    );
    assert_eq!(finalized["state"], "finalized");
    let finalize_status = mcp.call_tool(
        operation(mutation, "status"),
        json!({"operation_id": "e2e/finalize"}),
    );
    assert_eq!(finalize_status["state"], "finalized");

    let snapshot = mcp.call_tool(
        "scx_agent_classed.rules_snapshot",
        json!({
            "context": {"episode_id": 7, "operation_id": "e2e/snapshot"},
            "arguments": {"comms": [COMM]}
        }),
    );
    assert_eq!(snapshot["data"]["revision"], 3);
    assert_eq!(snapshot["data"]["rules_seq"], 6);
    assert_eq!(snapshot["data"]["rules"][0]["active_class"], "latency");
    assert_eq!(snapshot["data"]["rules"][0]["persisted_class"], "latency");
    assert_eq!(snapshot["data"]["rules"][0]["consistent"], true);

    mcp.finish();

    let state = scheduler.state.lock().unwrap();
    assert_eq!(state.server_error, None);
    assert_eq!(state.active.get(COMM), Some(&RuleClass::Latency));
    assert_eq!(
        state.cas_calls,
        vec![
            CasCall::new(RuleState::absent(), RuleState::present(RuleClass::Latency)),
            CasCall::new(RuleState::present(RuleClass::Latency), RuleState::absent()),
            CasCall::new(RuleState::absent(), RuleState::present(RuleClass::Latency)),
        ]
    );
    drop(state);

    let persisted = read_document(&scheduler.rules_path()).unwrap();
    assert_eq!(persisted.revision, 3);
    assert_eq!(persisted.rules.len(), 1);
    assert_eq!(persisted.rules[0].comm, COMM);
    assert_eq!(persisted.rules[0].class, RuleClass::Latency);
    assert!(journal.is_file());
}

fn operation<'a>(capability: &'a Value, phase: &str) -> &'a str {
    capability["operations"][phase]
        .as_str()
        .unwrap_or_else(|| panic!("missing mutation operation '{phase}'"))
}

fn pin_prepared(manifest: &Value, raw: Value) -> Value {
    let remote_provider = manifest["provider"]["id"].as_str().unwrap();
    let provider_version = manifest["provider"]["version"].as_str().unwrap();
    let baseline = raw["baseline"]["value"].clone();
    let desired = raw["desired"]["value"].clone();
    json!({
        "capability_id": "mcp/e2e/rule.upsert.v1",
        "provider": {
            "provider_id": format!("mcp/e2e/{remote_provider}"),
            "provider_version": provider_version,
            "provider_class": "mcp",
            "manifest_digest": digest_value(manifest)
        },
        "resource": format!(
            "mcp/e2e/{remote_provider}/{}",
            raw["resource"].as_str().unwrap()
        ),
        "baseline": state_with_digest(baseline),
        "desired": state_with_digest(desired),
        "driver_data": raw["driver_data"].clone()
    })
}

fn state_with_digest(value: Value) -> Value {
    json!({"digest": digest_value(&value), "value": value})
}

fn digest_value(value: &Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(value).unwrap());
    format!("sha256:{digest:x}")
}

fn operation_request(operation_id: &str, prepared: &Value) -> Value {
    json!({"operation_id": operation_id, "prepared": prepared})
}

fn verify_request(operation_id: &str, prepared: &Value, expected: &Value) -> Value {
    json!({
        "operation_id": operation_id,
        "prepared": prepared,
        "expected": expected
    })
}

struct McpProcess {
    child: Child,
    input: Option<BufWriter<ChildStdin>>,
    output: Option<BufReader<ChildStdout>>,
    next_id: u64,
}

impl McpProcess {
    fn start(control_socket: &Path, journal: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_scx_agent_classed_mcp"))
            .arg("--control-socket")
            .arg(control_socket)
            .arg("--journal")
            .arg(journal)
            .arg("--control-timeout-ms")
            .arg("1000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start real scx_agent_classed_mcp binary");
        let input = BufWriter::new(child.stdin.take().unwrap());
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input: Some(input),
            output: Some(output),
            next_id: 1,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &request).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();

        let mut line = String::new();
        let read = self.output.as_mut().unwrap().read_line(&mut line).unwrap();
        assert_ne!(read, 0, "MCP process closed stdout before replying");
        let response: Value = serde_json::from_str(&line).expect("valid MCP JSON-RPC response");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert!(
            response.get("error").is_none(),
            "MCP RPC failed: {}",
            response["error"]
        );
        response["result"].clone()
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        assert_ne!(
            result["isError"], true,
            "MCP tool '{name}' failed: {}",
            result["content"]
        );
        result["structuredContent"].clone()
    }

    fn finish(mut self) {
        drop(self.input.take());
        drop(self.output.take());
        let status = self.child.wait().expect("wait for MCP process");
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(status.success(), "MCP process failed: {stderr}");
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CasCall {
    expected: RuleState,
    desired: RuleState,
}

impl CasCall {
    fn new(expected: RuleState, desired: RuleState) -> Self {
        Self { expected, desired }
    }
}

struct FakeState {
    active: BTreeMap<String, RuleClass>,
    revision: u64,
    rules_seq: u64,
    cas_calls: Vec<CasCall>,
    server_error: Option<String>,
}

struct FakeScheduler {
    socket: PathBuf,
    rules: PathBuf,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeScheduler {
    fn start(root: &TestDir) -> Self {
        let socket = root.path().join("control.sock");
        let rules = root.path().join("learned-rules.json");
        write_document(&rules, 0, &BTreeMap::new()).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(FakeState {
            active: BTreeMap::new(),
            revision: 0,
            rules_seq: 0,
            cas_calls: Vec::new(),
            server_error: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread_rules = rules.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = serve_control(stream, &thread_state, &thread_rules) {
                            thread_state.lock().unwrap().server_error = Some(error);
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => {
                        thread_state.lock().unwrap().server_error = Some(error.to_string());
                        break;
                    }
                }
            }
        });
        Self {
            socket,
            rules,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn rules_path(&self) -> PathBuf {
        self.rules.clone()
    }
}

impl Drop for FakeScheduler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve_control(
    mut stream: UnixStream,
    state: &Arc<Mutex<FakeState>>,
    rules_path: &Path,
) -> Result<(), String> {
    let mut line = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    let request: ControlRequest = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    let response = handle_control(request, state, rules_path)?;
    serde_json::to_writer(&mut stream, &response).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn handle_control(
    request: ControlRequest,
    state: &Arc<Mutex<FakeState>>,
    rules_path: &Path,
) -> Result<ControlResponse, String> {
    if request.version != CONTROL_VERSION {
        return Err(format!("unexpected control version {}", request.version));
    }
    let mut state = state.lock().unwrap();
    let (status, comms, current) = match request.op {
        ControlOp::GetRule => {
            require_fields(&request, true, false, false, false)?;
            let comm = request.comm.clone().unwrap();
            let current = rule_state(state.active.get(&comm).copied());
            (ControlStatus::Ok, vec![comm], Some(current))
        }
        ControlOp::Snapshot => {
            require_fields(&request, false, true, false, false)?;
            let comms = request
                .comms
                .clone()
                .filter(|comms| !comms.is_empty())
                .ok_or_else(|| "snapshot is missing comms".to_string())?;
            (ControlStatus::Ok, comms, None)
        }
        ControlOp::CompareAndSetRule => {
            require_fields(&request, true, false, true, true)?;
            let comm = request.comm.clone().unwrap();
            let expected = request.expected.clone().unwrap();
            let desired = request.desired.clone().unwrap();
            if !expected.is_valid() || !desired.is_valid() {
                return Err("CAS contains an invalid rule state".into());
            }
            state
                .cas_calls
                .push(CasCall::new(expected.clone(), desired.clone()));
            let observed = rule_state(state.active.get(&comm).copied());
            let status = if observed != expected {
                ControlStatus::Conflict
            } else if observed == desired {
                ControlStatus::Noop
            } else {
                let mut next = state.active.clone();
                match desired.class {
                    Some(class) => {
                        next.insert(comm.clone(), class);
                    }
                    None => {
                        next.remove(&comm);
                    }
                }
                let revision = state.revision + 1;
                write_document(rules_path, revision, &next)?;
                let persisted = document_map(&read_document(rules_path)?)?;
                if persisted != next {
                    return Err("persisted CAS readback differs from desired table".into());
                }
                state.active = next;
                state.revision = revision;
                state.rules_seq += 2;
                ControlStatus::Applied
            };
            let current = rule_state(state.active.get(&comm).copied());
            (status, vec![comm], Some(current))
        }
    };

    let persisted_document = read_document(rules_path)?;
    if persisted_document.revision != state.revision {
        return Err("active and persisted revisions differ".into());
    }
    let persisted = document_map(&persisted_document)?;
    let rules = comms
        .iter()
        .map(|comm| observation(comm, &state.active, &persisted))
        .collect();
    let digest = Sha256::digest(fs::read(rules_path).map_err(|error| error.to_string())?);
    Ok(ControlResponse {
        version: CONTROL_VERSION,
        request_id: request.request_id,
        status,
        current,
        rules,
        revision: state.revision,
        rules_seq: state.rules_seq,
        effective_digest: format!("sha256:{digest:x}"),
        stats: Some(ControlStats {
            task_state_errors: 0,
            rule_refresh_deferred: 0,
        }),
        workload_fingerprint: Some("fake-scheduler-e2e".into()),
        message: None,
    })
}

fn require_fields(
    request: &ControlRequest,
    comm: bool,
    comms: bool,
    expected: bool,
    desired: bool,
) -> Result<(), String> {
    if request.comm.is_some() != comm
        || request.comms.is_some() != comms
        || request.expected.is_some() != expected
        || request.desired.is_some() != desired
    {
        return Err(format!("invalid fields for control op {:?}", request.op));
    }
    Ok(())
}

fn rule_state(class: Option<RuleClass>) -> RuleState {
    class.map_or_else(RuleState::absent, RuleState::present)
}

fn observation(
    comm: &str,
    active: &BTreeMap<String, RuleClass>,
    persisted: &BTreeMap<String, RuleClass>,
) -> RuleObservation {
    let active_class = active.get(comm).copied();
    let persisted_class = persisted.get(comm).copied();
    RuleObservation {
        comm: comm.into(),
        class: active_class.or(persisted_class).unwrap_or(RuleClass::Batch),
        source: if active_class.is_some() || persisted_class.is_some() {
            RuleSource::Learned
        } else {
            RuleSource::Default
        },
        active_class,
        persisted_class,
        consistent: active_class == persisted_class,
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredDocument {
    schema_version: u32,
    revision: u64,
    rules: Vec<StoredRule>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredRule {
    comm: String,
    class: RuleClass,
}

fn write_document(
    path: &Path,
    revision: u64,
    rules: &BTreeMap<String, RuleClass>,
) -> Result<(), String> {
    let document = StoredDocument {
        schema_version: 1,
        revision,
        rules: rules
            .iter()
            .map(|(comm, class)| StoredRule {
                comm: comm.clone(),
                class: *class,
            })
            .collect(),
    };
    let temporary = path.with_extension("tmp");
    let mut bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    Ok(())
}

fn read_document(path: &Path) -> Result<StoredDocument, String> {
    let document: StoredDocument =
        serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    if document.schema_version != 1 {
        return Err(format!(
            "unexpected learned-rule schema version {}",
            document.schema_version
        ));
    }
    Ok(document)
}

fn document_map(document: &StoredDocument) -> Result<BTreeMap<String, RuleClass>, String> {
    let mut rules = BTreeMap::new();
    for rule in &document.rules {
        if rules.insert(rule.comm.clone(), rule.class).is_some() {
            return Err(format!("duplicate persisted comm '{}'", rule.comm));
        }
    }
    Ok(rules)
}

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "scx-agent-classed-mcp-e2e-{}-{nonce}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
