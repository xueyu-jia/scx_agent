use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::activation::{ActivationEvent, ActivationRequest, ActivationResponse};

const MAX_ACTIVATION_BYTES: u64 = 64 * 1024;
const MAX_CONNECTIONS_PER_POLL: usize = 16;
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(500);

pub struct UnixIpcSource {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
}

pub struct UnixActivation {
    pub request: ActivationRequest,
    stream: Option<UnixStream>,
}

impl UnixActivation {
    fn new(request: ActivationRequest, stream: Option<UnixStream>) -> Self {
        Self { request, stream }
    }

    pub fn wants_response(&self) -> bool {
        self.request.wait
    }

    pub fn respond(mut self, response: &ActivationResponse) -> std::io::Result<()> {
        let Some(mut stream) = self.stream.take() else {
            return Ok(());
        };
        let payload = serde_json::to_vec(response)?;
        stream.write_all(&payload)?;
        stream.shutdown(std::net::Shutdown::Write)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

impl UnixIpcSource {
    pub fn bind(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!(
                            "activation socket '{}' is already served by another process",
                            path.display()
                        ),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&path)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to replace non-socket activation path '{}'",
                        path.display()
                    ),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        if let Err(error) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() {
            let _ = fs::remove_file(&path);
            return Err(std::io::Error::other(
                "activation socket path changed during bind",
            ));
        }
        Ok(Self {
            listener,
            path,
            identity: SocketIdentity::from_metadata(&metadata),
        })
    }

    pub fn poll(&mut self) -> std::io::Result<Vec<UnixActivation>> {
        let mut activations = Vec::new();
        for _ in 0..MAX_CONNECTIONS_PER_POLL {
            match self.listener.accept() {
                Ok((mut stream, _addr)) => {
                    if stream.set_read_timeout(Some(STREAM_READ_TIMEOUT)).is_err() {
                        continue;
                    }
                    // A bad client is isolated to this connection. Listener errors
                    // still propagate because they indicate a source-level fault.
                    if let Ok(request) = read_activation(&mut stream) {
                        let stream = if request.wait { Some(stream) } else { None };
                        activations.push(UnixActivation::new(request, stream));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        Ok(activations)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_activation(reader: &mut impl Read) -> std::io::Result<ActivationRequest> {
    let mut payload = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                payload.extend_from_slice(&buffer[..n]);
                if payload.len() as u64 > MAX_ACTIVATION_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "activation event exceeds 64 KiB",
                    ));
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if payload.is_empty() {
                    return Err(error);
                }
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if payload.len() as u64 > MAX_ACTIVATION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "activation event exceeds 64 KiB",
        ));
    }
    let value =
        serde_json::from_slice::<serde_json::Value>(&payload).map_err(std::io::Error::other)?;
    if value.get("event").is_some() {
        serde_json::from_value::<ActivationRequest>(value).map_err(std::io::Error::other)
    } else {
        serde_json::from_value::<ActivationEvent>(value)
            .map(ActivationRequest::fire_and_forget)
            .map_err(std::io::Error::other)
    }
}

impl Drop for UnixIpcSource {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && SocketIdentity::from_metadata(&metadata) == self.identity
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn send_unix_activation(
    path: impl AsRef<Path>,
    event: &ActivationEvent,
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    let payload = serde_json::to_vec(event)?;
    if payload.len() as u64 > MAX_ACTIVATION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "activation event exceeds 64 KiB",
        ));
    }
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)
}

pub fn send_unix_activation_request(
    path: impl AsRef<Path>,
    request: &ActivationRequest,
    timeout: Duration,
) -> std::io::Result<ActivationResponse> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    let payload = serde_json::to_vec(request)?;
    if payload.len() as u64 > MAX_ACTIVATION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "activation request exceeds 64 KiB",
        ));
    }
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = Vec::new();
    stream
        .take(MAX_ACTIVATION_BYTES + 1)
        .read_to_end(&mut response)?;
    if response.len() as u64 > MAX_ACTIVATION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "activation response exceeds 64 KiB",
        ));
    }
    serde_json::from_slice::<ActivationResponse>(&response).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::activation::{
        ActivationEvent, ActivationOutcomeStatus, ActivationRequest, ActivationResponse,
        EventSource, Scope, Severity,
    };

    use super::*;

    #[test]
    fn unix_ipc_source_receives_activation_event() {
        let path =
            std::env::temp_dir().join(format!("tuning-agent-test-{}.sock", std::process::id()));
        let mut source = UnixIpcSource::bind(&path).expect("bind unix source");
        let second_bind = UnixIpcSource::bind(&path);
        assert!(matches!(
            second_bind,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse
        ));
        let event = ActivationEvent::new(
            EventSource::Cli,
            "manual".to_string(),
            Severity::Info,
            Scope::Host,
        );

        send_unix_activation(&path, &event).expect("send event");
        let events = poll_until_event(&mut source);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request.event.event_type, "manual");
        assert!(!events[0].wants_response());
    }

    #[test]
    fn unix_ipc_source_answers_waiting_activation_request() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-wait-test-{}.sock",
            std::process::id()
        ));
        let mut source = UnixIpcSource::bind(&path).expect("bind unix source");
        let event = ActivationEvent::new(
            EventSource::Cli,
            "manual".to_string(),
            Severity::Info,
            Scope::Host,
        );
        let request = ActivationRequest::new("request-1".into(), true, event);
        let client_path = path.clone();
        let client = thread::spawn(move || {
            send_unix_activation_request(client_path, &request, Duration::from_secs(5))
                .expect("wait response")
        });

        let mut events = poll_until_event(&mut source);
        assert_eq!(events.len(), 1);
        assert!(events[0].wants_response());
        events
            .pop()
            .unwrap()
            .respond(&ActivationResponse::rejected(
                "request-1".into(),
                "test rejection".into(),
            ))
            .expect("send response");
        let response = client.join().expect("client thread joined");

        assert_eq!(response.request_id, "request-1");
        assert_eq!(response.status, ActivationOutcomeStatus::Rejected);
        assert_eq!(response.error.as_deref(), Some("test rejection"));
    }

    #[test]
    fn malformed_and_oversized_connections_are_rejected_independently() {
        let mut malformed = Cursor::new(b"not-json".to_vec());
        assert_eq!(
            read_activation(&mut malformed).unwrap_err().kind(),
            std::io::ErrorKind::Other
        );

        let mut oversized = Cursor::new(vec![b'x'; MAX_ACTIVATION_BYTES as usize + 1]);
        assert_eq!(
            read_activation(&mut oversized).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    fn poll_until_event(source: &mut UnixIpcSource) -> Vec<UnixActivation> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = source.poll().expect("poll events");
            if !events.is_empty() {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for unix activation"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
