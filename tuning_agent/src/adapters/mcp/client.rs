use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::mcp::{McpAdapterError, McpAdapterErrorKind};
use crate::config::McpServerConfig;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_TRANSPORT_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_TAIL_BYTES: usize = 16 * 1024;
const ENVELOPE_ALLOWANCE_BYTES: usize = 64 * 1024;
const MAX_REQUEST_TIMEOUT_MS: u64 = 300_000;

type PendingResponse = Result<Value, McpAdapterError>;
type PendingMap = Arc<Mutex<HashMap<u64, Sender<PendingResponse>>>>;

#[derive(Clone)]
pub(crate) struct McpStdioClient {
    core: Arc<ClientCore>,
}

struct ClientCore {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: PendingMap,
    next_id: AtomicU64,
    request_timeout: Duration,
    closed: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr_reader: Mutex<Option<JoinHandle<()>>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
}

impl McpStdioClient {
    pub(crate) fn connect(config: &McpServerConfig) -> Result<Self, McpAdapterError> {
        validate_process_config(config)?;

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .env_clear()
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Spawn,
                format!(
                    "failed to spawn MCP server '{}' using '{}': {error}",
                    config.id, config.command
                ),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            McpAdapterError::new(McpAdapterErrorKind::Spawn, "MCP child stdin was not piped")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            McpAdapterError::new(McpAdapterErrorKind::Spawn, "MCP child stdout was not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            McpAdapterError::new(McpAdapterErrorKind::Spawn, "MCP child stderr was not piped")
        })?;

        let pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = pending.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = closed.clone();
        let reader = thread::Builder::new()
            .name(format!("mcp-{}-stdout", config.id))
            .spawn(move || read_responses(stdout, reader_pending, reader_closed))
            .map_err(|error| {
                McpAdapterError::new(
                    McpAdapterErrorKind::Spawn,
                    format!("failed to start MCP stdout reader: {error}"),
                )
            })?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let drain_tail = stderr_tail.clone();
        let stderr_reader = thread::Builder::new()
            .name(format!("mcp-{}-stderr", config.id))
            .spawn(move || drain_stderr(stderr, drain_tail))
            .map_err(|error| {
                McpAdapterError::new(
                    McpAdapterErrorKind::Spawn,
                    format!("failed to start MCP stderr reader: {error}"),
                )
            })?;

        let client = Self {
            core: Arc::new(ClientCore {
                stdin: Mutex::new(stdin),
                child: Mutex::new(child),
                pending,
                next_id: AtomicU64::new(1),
                request_timeout: Duration::from_millis(config.request_timeout_ms),
                closed,
                reader: Mutex::new(Some(reader)),
                stderr_reader: Mutex::new(Some(stderr_reader)),
                stderr_tail,
            }),
        };
        client.initialize()?;
        Ok(client)
    }

    pub(crate) fn request_timeout(&self) -> Duration {
        self.core.request_timeout
    }

    pub(crate) fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpAdapterError> {
        if self.core.closed.load(Ordering::Acquire) {
            return Err(self.unavailable("MCP connection is closed"));
        }
        if timeout.is_zero() {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::InvalidConfig,
                "MCP request timeout must be greater than zero",
            ));
        }
        let id = self.core.next_id.fetch_add(1, Ordering::Relaxed);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let payload = serde_json::to_vec(&request).map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("failed to encode MCP request '{method}': {error}"),
            )
        })?;
        if payload.len() > MAX_TRANSPORT_FRAME_BYTES {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("MCP request '{method}' exceeds the transport frame limit"),
            ));
        }

        let (sender, receiver) = mpsc::channel();
        self.core
            .pending
            .lock()
            .map_err(|_| self.internal("MCP pending-response lock is poisoned"))?
            .insert(id, sender);
        if self.core.closed.load(Ordering::Acquire) {
            self.remove_pending(id);
            return Err(self.unavailable("MCP connection closed before request dispatch"));
        }

        let write_result = self
            .core
            .stdin
            .lock()
            .map_err(|_| self.internal("MCP stdin lock is poisoned"))
            .and_then(|mut stdin| {
                write_frame(&mut *stdin, &payload).map_err(|error| {
                    self.unavailable(format!("failed to write MCP request '{method}': {error}"))
                })
            });
        if let Err(error) = write_result {
            self.core.closed.store(true, Ordering::Release);
            self.remove_pending(id);
            return Err(error);
        }

        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.remove_pending(id);
                Err(McpAdapterError::new(
                    McpAdapterErrorKind::Timeout,
                    format!(
                        "MCP request '{method}' timed out after {} ms",
                        timeout.as_millis()
                    ),
                )
                .retryable(true))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.remove_pending(id);
                Err(self.unavailable(format!(
                    "MCP response channel closed while waiting for '{method}'"
                )))
            }
        }
    }

    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), McpAdapterError> {
        if self.core.closed.load(Ordering::Acquire) {
            return Err(self.unavailable("MCP connection is closed"));
        }
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let payload = serde_json::to_vec(&notification).map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("failed to encode MCP notification '{method}': {error}"),
            )
        })?;
        self.core
            .stdin
            .lock()
            .map_err(|_| self.internal("MCP stdin lock is poisoned"))
            .and_then(|mut stdin| {
                write_frame(&mut *stdin, &payload).map_err(|error| {
                    self.unavailable(format!(
                        "failed to write MCP notification '{method}': {error}"
                    ))
                })
            })
    }

    pub(crate) fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<Value, McpAdapterError> {
        if !arguments.is_object() {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("MCP tool '{name}' arguments must serialize to an object"),
            ));
        }
        let result = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
            timeout.min(self.core.request_timeout),
        )?;
        decode_tool_result(name, result, max_output_bytes)
    }

    fn initialize(&self) -> Result<(), McpAdapterError> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "tuning-agent",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
            self.core.request_timeout,
        )?;
        validate_initialize_result(&result)?;
        self.notify("notifications/initialized", json!({}))
    }

    fn remove_pending(&self, id: u64) {
        if let Ok(mut pending) = self.core.pending.lock() {
            pending.remove(&id);
        }
    }

    fn stderr_tail(&self) -> String {
        self.core
            .stderr_tail
            .lock()
            .ok()
            .map(|tail| String::from_utf8_lossy(&tail).trim().to_string())
            .filter(|tail| !tail.is_empty())
            .map(|tail| format!("; server stderr: {tail}"))
            .unwrap_or_default()
    }

    fn unavailable(&self, message: impl Into<String>) -> McpAdapterError {
        McpAdapterError::new(
            McpAdapterErrorKind::Io,
            format!("{}{}", message.into(), self.stderr_tail()),
        )
        .retryable(true)
    }

    fn internal(&self, message: impl Into<String>) -> McpAdapterError {
        McpAdapterError::new(McpAdapterErrorKind::Io, message).retryable(true)
    }
}

impl Drop for ClientCore {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        fail_all_pending(
            &self.pending,
            McpAdapterError::new(McpAdapterErrorKind::Io, "MCP client was dropped").retryable(true),
        );
        join_if_finished(&self.reader);
        join_if_finished(&self.stderr_reader);
    }
}

fn validate_process_config(config: &McpServerConfig) -> Result<(), McpAdapterError> {
    if !config.enabled {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            format!("MCP server '{}' is disabled", config.id),
        ));
    }
    if config.id.trim().is_empty() || config.id.trim() != config.id {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            "MCP server id must be non-empty and have no surrounding whitespace",
        ));
    }
    if config.command.trim().is_empty() {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            format!("MCP server '{}' has no command", config.id),
        ));
    }
    if !Path::new(&config.command).is_absolute() {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            format!(
                "MCP server '{}' command must be an absolute path; PATH lookup is forbidden",
                config.id
            ),
        ));
    }
    if config.request_timeout_ms == 0 || config.request_timeout_ms > MAX_REQUEST_TIMEOUT_MS {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            format!(
                "MCP server '{}' request timeout must be within 1..={MAX_REQUEST_TIMEOUT_MS} ms",
                config.id
            ),
        ));
    }
    for key in config.env.keys() {
        if key.is_empty() || key.contains('=') || key.chars().any(char::is_control) {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::InvalidConfig,
                format!(
                    "MCP server '{}' has invalid environment key '{key}'",
                    config.id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_initialize_result(result: &Value) -> Result<(), McpAdapterError> {
    let object = result.as_object().ok_or_else(|| {
        McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP initialize result must be an object",
        )
    })?;
    let version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                "MCP initialize result is missing protocolVersion",
            )
        })?;
    if version != MCP_PROTOCOL_VERSION {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            format!(
                "MCP server selected unsupported protocol version '{version}', expected '{MCP_PROTOCOL_VERSION}'"
            ),
        ));
    }
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                "MCP initialize result is missing capabilities",
            )
        })?;
    if !capabilities.get("tools").is_some_and(Value::is_object)
        || !capabilities.get("resources").is_some_and(Value::is_object)
    {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP server must advertise tools and resources capabilities",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallEnvelope {
    #[serde(default)]
    content: Vec<ToolContent>,
    #[serde(default)]
    structured_content: Option<Value>,
    #[serde(default)]
    is_error: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ToolContent {
    Text {
        text: String,
    },
    #[serde(other)]
    Unsupported,
}

fn decode_tool_result(
    tool: &str,
    raw: Value,
    max_output_bytes: usize,
) -> Result<Value, McpAdapterError> {
    if max_output_bytes == 0 {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::InvalidConfig,
            format!("MCP tool '{tool}' has a zero output limit"),
        ));
    }
    let encoded_len = serde_json::to_vec(&raw)
        .map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("failed to size MCP tool '{tool}' result: {error}"),
            )
        })?
        .len();
    if encoded_len > max_output_bytes.saturating_add(ENVELOPE_ALLOWANCE_BYTES) {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            format!("MCP tool '{tool}' response exceeded its output limit: {encoded_len} bytes"),
        ));
    }
    let envelope: ToolCallEnvelope = serde_json::from_value(raw).map_err(|error| {
        McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            format!("invalid MCP tools/call result for '{tool}': {error}"),
        )
    })?;
    if envelope.is_error {
        let message = envelope
            .content
            .iter()
            .filter_map(|content| match content {
                ToolContent::Text { text } => Some(text.as_str()),
                ToolContent::Unsupported => None,
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Tool,
            if message.is_empty() {
                format!("MCP tool '{tool}' reported an error")
            } else {
                format!("MCP tool '{tool}' reported an error: {message}")
            },
        ));
    }
    let value = match envelope.structured_content {
        Some(value) => value,
        None => {
            if envelope.content.len() != 1 {
                return Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    format!(
                        "MCP tool '{tool}' must return exactly one text block when structuredContent is absent"
                    ),
                ));
            }
            let first = match envelope.content.into_iter().next() {
                Some(ToolContent::Text { text }) => text,
                Some(ToolContent::Unsupported) | None => {
                    return Err(McpAdapterError::new(
                        McpAdapterErrorKind::Protocol,
                        format!(
                            "MCP tool '{tool}' returned unsupported content instead of structured JSON"
                        ),
                    ));
                }
            };
            if first.is_empty() {
                return Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    format!("MCP tool '{tool}' returned an empty JSON text block"),
                ));
            }
            serde_json::from_str(&first).map_err(|error| {
                McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    format!("MCP tool '{tool}' text content is not JSON: {error}"),
                )
            })?
        }
    };
    let output_len = serde_json::to_vec(&value)
        .map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("failed to encode MCP tool '{tool}' structured output: {error}"),
            )
        })?
        .len();
    if output_len > max_output_bytes {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            format!(
                "MCP tool '{tool}' structured output exceeded its limit: {output_len} > {max_output_bytes} bytes"
            ),
        ));
    }
    Ok(value)
}

fn read_responses(stdout: impl Read, pending: PendingMap, closed: Arc<AtomicBool>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader, MAX_TRANSPORT_FRAME_BYTES) {
            Ok(Some(payload)) => match parse_response(&payload) {
                Ok(Some((id, result))) => {
                    if let Ok(mut map) = pending.lock() {
                        if let Some(sender) = map.remove(&id) {
                            let _ = sender.send(result);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    closed.store(true, Ordering::Release);
                    fail_all_pending(&pending, error);
                    return;
                }
            },
            Ok(None) => {
                closed.store(true, Ordering::Release);
                fail_all_pending(
                    &pending,
                    McpAdapterError::new(McpAdapterErrorKind::Io, "MCP server closed stdout")
                        .retryable(true),
                );
                return;
            }
            Err(error) => {
                closed.store(true, Ordering::Release);
                fail_all_pending(&pending, error);
                return;
            }
        }
    }
}

fn parse_response(payload: &[u8]) -> Result<Option<(u64, PendingResponse)>, McpAdapterError> {
    let value: Value = serde_json::from_slice(payload).map_err(|error| {
        McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            format!("MCP frame is not valid JSON: {error}"),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP JSON-RPC message must be an object",
        )
    })?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP message does not declare jsonrpc=2.0",
        ));
    }
    let Some(id_value) = object.get("id") else {
        // Notifications and server-originated requests are not correlated responses.
        return Ok(None);
    };
    let id = id_value.as_u64().ok_or_else(|| {
        McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP response id must echo the numeric client request id",
        )
    })?;
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(Some((id, Ok(result.clone())))),
        (None, Some(error)) => {
            let error = error.as_object().ok_or_else(|| {
                McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    "MCP JSON-RPC error member must be an object",
                )
            })?;
            let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
                McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    "MCP JSON-RPC error object is missing an integer code",
                )
            })?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    McpAdapterError::new(
                        McpAdapterErrorKind::Protocol,
                        "MCP JSON-RPC error object is missing a string message",
                    )
                })?;
            Ok(Some((
                id,
                Err(McpAdapterError::new(
                    McpAdapterErrorKind::Rpc,
                    format!("MCP JSON-RPC error {code}: {message}"),
                )),
            )))
        }
        _ => Err(McpAdapterError::new(
            McpAdapterErrorKind::Protocol,
            "MCP response must contain exactly one of result or error",
        )),
    }
}

pub(crate) fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MCP newline-delimited JSON payload contains a raw line break",
        ));
    }
    writer.write_all(payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub(crate) fn read_frame(
    reader: &mut impl BufRead,
    max_payload_bytes: usize,
) -> Result<Option<Vec<u8>>, McpAdapterError> {
    let mut payload = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| {
            McpAdapterError::new(
                McpAdapterErrorKind::Io,
                format!("failed to read MCP newline-delimited frame: {error}"),
            )
            .retryable(true)
        })?;
        if available.is_empty() {
            return if payload.is_empty() {
                Ok(None)
            } else {
                Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    "MCP stream ended before the frame newline",
                ))
            };
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if payload.len().saturating_add(newline) > max_payload_bytes {
                return Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    format!("MCP frame payload exceeds limit: more than {max_payload_bytes} bytes"),
                ));
            }
            payload.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if payload.is_empty() {
                return Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    "MCP frame payload must not be empty",
                ));
            }
            if payload.contains(&b'\r') {
                return Err(McpAdapterError::new(
                    McpAdapterErrorKind::Protocol,
                    "MCP newline-delimited JSON payload contains a raw carriage return",
                ));
            }
            return Ok(Some(payload));
        }

        if payload.len().saturating_add(available.len()) > max_payload_bytes {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::Protocol,
                format!("MCP frame payload exceeds limit: more than {max_payload_bytes} bytes"),
            ));
        }
        let count = available.len();
        payload.extend_from_slice(available);
        reader.consume(count);
    }
}

fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0_u8; 4096];
    loop {
        let count = match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        if let Ok(mut retained) = tail.lock() {
            retained.extend_from_slice(&buffer[..count]);
            if retained.len() > MAX_STDERR_TAIL_BYTES {
                let excess = retained.len() - MAX_STDERR_TAIL_BYTES;
                retained.drain(..excess);
            }
        }
    }
}

fn fail_all_pending(pending: &PendingMap, error: McpAdapterError) {
    let senders = pending
        .lock()
        .map(|mut pending| {
            pending
                .drain()
                .map(|(_, sender)| sender)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

fn join_if_finished(handle: &Mutex<Option<JoinHandle<()>>>) {
    if let Ok(mut handle) = handle.lock() {
        if handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = handle.take() {
                let _ = handle.join();
            }
        } else {
            // Dropping a blocked reader handle detaches it. The child has already been killed
            // and waited, so its pipe normally reaches EOF immediately.
            handle.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn newline_frame_round_trips_json_payload() {
        let payload = br#"{"jsonrpc":"2.0","id":7,"result":{"value":"x"}}"#;
        let mut encoded = Vec::new();
        write_frame(&mut encoded, payload).unwrap();

        let decoded = read_frame(&mut Cursor::new(encoded), 1024)
            .unwrap()
            .unwrap();

        assert_eq!(decoded, payload);
    }

    #[test]
    fn frame_rejects_raw_line_breaks_and_oversized_payload() {
        let mut encoded = Vec::new();
        assert_eq!(
            write_frame(&mut encoded, b"{\n}").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let oversized = b"12345678901\n";
        assert_eq!(
            read_frame(&mut Cursor::new(oversized), 10)
                .unwrap_err()
                .kind,
            McpAdapterErrorKind::Protocol
        );
    }

    #[test]
    fn response_parser_correlates_numeric_ids_and_preserves_rpc_errors() {
        let parsed = parse_response(br#"{"jsonrpc":"2.0","id":42,"result":{"ok":true}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.0, 42);
        assert_eq!(parsed.1.unwrap()["ok"], true);

        let parsed = parse_response(
            br#"{"jsonrpc":"2.0","id":9,"error":{"code":-32000,"message":"failed"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.0, 9);
        assert_eq!(parsed.1.unwrap_err().kind, McpAdapterErrorKind::Rpc);
    }

    #[test]
    fn tool_result_prefers_structured_content_and_enforces_output_limit() {
        let raw = json!({
            "content": [{"type": "text", "text": "ignored"}],
            "structuredContent": {"value": 7},
            "isError": false
        });
        assert_eq!(
            decode_tool_result("sample", raw.clone(), 64).unwrap()["value"],
            7
        );
        assert_eq!(
            decode_tool_result("sample", raw, 1).unwrap_err().kind,
            McpAdapterErrorKind::Protocol
        );

        let mixed_fallback = json!({
            "content": [
                {"type": "text", "text": "{\"value\":7}"},
                {"type": "image", "data": "ignored"}
            ],
            "isError": false
        });
        assert_eq!(
            decode_tool_result("sample", mixed_fallback, 64)
                .unwrap_err()
                .kind,
            McpAdapterErrorKind::Protocol
        );
    }

    #[test]
    fn process_config_requires_an_absolute_command() {
        let config = McpServerConfig {
            id: "test".to_string(),
            command: "server-from-path".to_string(),
            ..McpServerConfig::default()
        };
        assert_eq!(
            validate_process_config(&config).unwrap_err().kind,
            McpAdapterErrorKind::InvalidConfig
        );
    }

    #[cfg(unix)]
    #[test]
    fn client_initializes_then_reuses_a_newline_stdio_connection() {
        let script = r#"
            IFS= read -r initialize || exit 10
            case "$initialize" in \{*) ;; *) exit 11 ;; esac
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"fixture","version":"1"}}}'
            IFS= read -r initialized || exit 12
            case "$initialized" in *notifications/initialized*) ;; *) exit 13 ;; esac
            IFS= read -r ping || exit 14
            case "$ping" in *\"method\":\"ping\"*) ;; *) exit 15 ;; esac
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"pong":true}}'
            IFS= read -r _keep_alive
        "#;
        let config = McpServerConfig {
            id: "fixture".to_string(),
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            request_timeout_ms: 1_000,
            ..McpServerConfig::default()
        };

        let client = McpStdioClient::connect(&config).unwrap();
        let response = client
            .request("ping", json!({}), Duration::from_millis(1_000))
            .unwrap();
        assert_eq!(response["pong"], true);
    }
}
