//! One JSON request line in, one JSON response line out, over a Unix socket.
use serde_json::Value;
use std::{
    fmt, io,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_REQUEST: usize = 16 * 1024;
pub const MAX_RESPONSE: u64 = 8 * 1024 * 1024;

pub fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("watchctl.sock"),
        _ => std::env::temp_dir().join(format!("watchctl-{}.sock", unsafe { libc::geteuid() })),
    }
}

#[derive(Debug)]
pub enum IpcError {
    Unreachable(PathBuf, io::Error),
    ForeignSocket(PathBuf),
    Io(io::Error),
    Protocol,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(path, error) => write!(
                f,
                "watchctld is not reachable at {} ({error}); start it with `watchctld`",
                path.display()
            ),
            Self::ForeignSocket(path) => {
                write!(f, "{} is not owned by the current user", path.display())
            }
            Self::Io(error) => write!(f, "daemon connection failed: {error}"),
            Self::Protocol => write!(f, "daemon sent a malformed response"),
        }
    }
}

impl From<io::Error> for IpcError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn call(socket: &Path, request: &Value) -> Result<Value, IpcError> {
    let metadata =
        std::fs::symlink_metadata(socket).map_err(|e| IpcError::Unreachable(socket.into(), e))?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(IpcError::ForeignSocket(socket.into()));
    }
    let mut stream =
        UnixStream::connect(socket).map_err(|e| IpcError::Unreachable(socket.into(), e))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = request.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut response = String::new();
    BufReader::new(stream.take(MAX_RESPONSE)).read_line(&mut response)?;
    serde_json::from_str(&response).map_err(|_| IpcError::Protocol)
}
