use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::activation::ActivationEvent;

pub struct UnixIpcSource {
    listener: UnixListener,
    path: PathBuf,
}

impl UnixIpcSource {
    pub fn bind(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, path })
    }

    pub fn poll(&mut self) -> std::io::Result<Vec<ActivationEvent>> {
        let mut events = Vec::new();
        loop {
            match self.listener.accept() {
                Ok((mut stream, _addr)) => {
                    let mut payload = String::new();
                    stream.read_to_string(&mut payload)?;
                    let event = serde_json::from_str::<ActivationEvent>(&payload)?;
                    events.push(event);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        Ok(events)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UnixIpcSource {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn send_unix_activation(
    path: impl AsRef<Path>,
    event: &ActivationEvent,
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    let payload = serde_json::to_vec(event)?;
    stream.write_all(&payload)?;
    stream.shutdown(std::net::Shutdown::Write)
}

#[cfg(test)]
mod tests {
    use crate::activation::{ActivationEvent, EventSource, Severity};
    use crate::types::Scope;

    use super::*;

    #[test]
    fn unix_ipc_source_receives_activation_event() {
        let path =
            std::env::temp_dir().join(format!("tuning-agent-test-{}.sock", std::process::id()));
        let mut source = UnixIpcSource::bind(&path).expect("bind unix source");
        let event = ActivationEvent::new(
            EventSource::Cli,
            "manual".to_string(),
            Severity::Info,
            Scope::Host,
        );

        send_unix_activation(&path, &event).expect("send event");
        let events = source.poll().expect("poll events");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "manual");
    }
}
