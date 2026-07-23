use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use socket2::{Domain, SockAddr, Socket, Type};

use crate::control_wire::{ControlRequest, ControlResponse};

const MAX_CONNECTIONS_PER_POLL: usize = 1;
const IO_TIMEOUT: Duration = Duration::from_millis(100);

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

pub struct ControlConnection {
    pub request: ControlRequest,
    stream: UnixStream,
}

impl ControlConnection {
    pub fn respond(mut self, response: &ControlResponse) -> Result<()> {
        let payload = serde_json::to_vec(response).context("encoding control response")?;
        if payload.len() as u64 > crate::control_wire::MAX_CONTROL_FRAME_BYTES {
            bail!(
                "control response exceeds {} bytes",
                crate::control_wire::MAX_CONTROL_FRAME_BYTES
            );
        }
        self.stream
            .write_all(&payload)
            .context("writing control response")?;
        self.stream
            .shutdown(std::net::Shutdown::Write)
            .context("closing control response")?;
        Ok(())
    }
}

pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
}

impl ControlServer {
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating control directory {}", parent.display()))?;
        }
        remove_stale_socket(&path)?;

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&path);
            return Err(error).context("setting control socket permissions");
        }
        listener
            .set_nonblocking(true)
            .context("setting control socket nonblocking")?;
        let metadata = fs::symlink_metadata(&path).context("reading control socket metadata")?;
        let identity = SocketIdentity::from_metadata(&metadata);

        Ok(Self {
            listener,
            path,
            identity,
        })
    }

    pub fn poll(&self) -> Result<Vec<ControlConnection>> {
        let mut connections = Vec::new();
        for _ in 0..MAX_CONNECTIONS_PER_POLL {
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error).context("accepting control connection"),
            };
            if let Err(error) = stream
                .set_read_timeout(Some(IO_TIMEOUT))
                .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
            {
                log::warn!("rejected control connection without bounded I/O: {error}");
                continue;
            }
            match read_request(&mut stream) {
                Ok(request) => connections.push(ControlConnection { request, stream }),
                Err(error) => log::warn!("rejected control request: {error:#}"),
            }
        }
        Ok(connections)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && SocketIdentity::from_metadata(&metadata) == self.identity
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => match connect_with_timeout(path) {
            Ok(_) => bail!("control socket {} is already in use", path.display()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(path)
                    .with_context(|| format!("removing stale socket {}", path.display()))?;
            }
            Err(error) => return Err(error).context("checking existing control socket"),
        },
        Ok(_) => bail!(
            "refusing to replace non-socket control path {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("checking control socket path"),
    }
    Ok(())
}

fn connect_with_timeout(path: &Path) -> std::io::Result<UnixStream> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.connect_timeout(&SockAddr::unix(path)?, IO_TIMEOUT)?;
    Ok(UnixStream::from(OwnedFd::from(socket)))
}

fn read_request(stream: &mut UnixStream) -> Result<ControlRequest> {
    let mut payload = Vec::new();
    BufReader::new(stream.take(crate::control_wire::MAX_CONTROL_FRAME_BYTES + 1))
        .read_until(b'\n', &mut payload)
        .context("reading control request")?;
    if payload.len() as u64 > crate::control_wire::MAX_CONTROL_FRAME_BYTES {
        bail!(
            "control request exceeds {} bytes",
            crate::control_wire::MAX_CONTROL_FRAME_BYTES
        );
    }
    serde_json::from_slice(&payload).context("decoding control request")
}
