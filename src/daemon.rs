//! watchctld: a single long-lived backend shared by short-lived clients.
//!
//! `LinuxBackend` stops the protocol worker when dropped, so the session has to
//! live in a process that outlives each `watchctl` invocation.
use crate::{ipc::MAX_REQUEST, paths::Paths};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Set from SIGINT/SIGTERM handlers.
pub static TERMINATE: AtomicBool = AtomicBool::new(false);

/// Backend methods forwarded unchanged; everything else is rejected.
pub const BACKEND_METHODS: &[&str] = &[
    "snapshot",
    "scan",
    "pair",
    "resume",
    "confirmSetup",
    "refreshAdapters",
    "selectAdapter",
    "connect",
    "submitPin",
    "retryActivation",
    "stop",
];

pub trait Backend: Send + 'static {
    fn command(&mut self, request: Value) -> Value;
}

impl Backend for watch_transport_linux::LinuxBackend {
    fn command(&mut self, request: Value) -> Value {
        watch_transport_linux::LinuxBackend::command(self, request)
    }
}

type Shared<B> = Arc<Mutex<Option<B>>>;

pub fn dispatch<B: Backend>(
    backend: &Mutex<Option<B>>,
    shutdown: &AtomicBool,
    line: &str,
) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return json!({"status":"INVALID_REQUEST"});
    };
    let method = request["method"].as_str().map(str::to_owned);
    match method.as_deref() {
        Some("daemonInfo") => {
            let mut info = json!({"status":"OK","daemonVersion":env!("CARGO_PKG_VERSION"),"pid":std::process::id()});
            info.as_object_mut()
                .unwrap()
                .extend(Paths::resolve().to_json().as_object().unwrap().clone());
            info
        }
        Some("shutdown") => {
            shutdown.store(true, Ordering::SeqCst);
            json!({"status":"QUEUED"})
        }
        Some(method) if BACKEND_METHODS.contains(&method) => {
            let mut guard = backend.lock().unwrap_or_else(|p| p.into_inner());
            match guard.as_mut() {
                Some(backend) => backend.command(request),
                None => json!({"status":"CORE_UNAVAILABLE"}),
            }
        }
        Some(_) => json!({"status":"UNSUPPORTED_OPERATION"}),
        None => json!({"status":"INVALID_REQUEST"}),
    }
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    (result == 0).then_some(credentials.uid)
}

fn handle<B: Backend>(
    backend: &Mutex<Option<B>>,
    shutdown: &AtomicBool,
    stream: UnixStream,
) -> io::Result<()> {
    if peer_uid(&stream) != Some(unsafe { libc::geteuid() }) {
        return Ok(());
    }
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST as u64 + 1)).read_line(&mut line)?;
    let response = if line.len() > MAX_REQUEST || !line.ends_with('\n') {
        // Drain so the client reads the rejection instead of a connection reset.
        let _ = io::copy(&mut (&stream).take(1 << 20), &mut io::sink());
        json!({"status":"INVALID_REQUEST"})
    } else {
        dispatch(backend, shutdown, &line)
    };
    let mut output = response.to_string();
    output.push('\n');
    (&stream).write_all(output.as_bytes())
}

/// Binds a user-only socket, replacing a stale socket but never a live daemon
/// or a non-socket file.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "another watchctld is already listening",
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "path exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let previous = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(path);
    unsafe { libc::umask(previous) };
    let listener = listener?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves until a `shutdown` request or [`TERMINATE`], then drops the backend
/// (which stops the protocol worker). `tick` drives `snapshot` so the backend
/// reaps its worker and resumes after setup confirmation even with no client.
pub fn serve<B: Backend>(listener: UnixListener, backend: B, tick: Duration) -> io::Result<()> {
    let backend: Shared<B> = Arc::new(Mutex::new(Some(backend)));
    let shutdown = Arc::new(AtomicBool::new(false));
    listener.set_nonblocking(true)?;
    let mut last_tick = Instant::now();
    let result = loop {
        if shutdown.load(Ordering::SeqCst) || TERMINATE.load(Ordering::SeqCst) {
            break Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_ok() {
                    let (backend, shutdown) = (Arc::clone(&backend), Arc::clone(&shutdown));
                    thread::spawn(move || {
                        let _ = handle(&backend, &shutdown, stream);
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => break Err(error),
        }
        if last_tick.elapsed() >= tick {
            last_tick = Instant::now();
            let mut guard = backend.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(backend) = guard.as_mut() {
                backend.command(json!({"method":"snapshot"}));
            }
        }
    };
    let finished = backend.lock().unwrap_or_else(|p| p.into_inner()).take();
    drop(finished);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;
    use std::{path::PathBuf, sync::atomic::AtomicUsize};

    struct Mock {
        calls: Arc<Mutex<Vec<Value>>>,
        dropped: Arc<AtomicBool>,
    }
    impl Backend for Mock {
        fn command(&mut self, request: Value) -> Value {
            self.calls.lock().unwrap().push(request.clone());
            match request["method"].as_str() {
                Some("snapshot") => json!({"apiVersion":1,"phase":"IDLE"}),
                _ => json!({"status":"QUEUED"}),
            }
        }
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    fn mock() -> (Mock, Arc<Mutex<Vec<Value>>>, Arc<AtomicBool>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicBool::new(false));
        let mock = Mock {
            calls: Arc::clone(&calls),
            dropped: Arc::clone(&dropped),
        };
        (mock, calls, dropped)
    }
    fn socket_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "watchctl-test-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ))
    }

    #[test]
    fn dispatch_forwards_only_backend_methods() {
        let (mock, calls, _) = mock();
        let backend = Mutex::new(Some(mock));
        let shutdown = AtomicBool::new(false);
        let r = dispatch(
            &backend,
            &shutdown,
            r#"{"method":"submitPin","pin":"123456"}"#,
        );
        assert_eq!(r["status"], "QUEUED");
        assert_eq!(calls.lock().unwrap()[0]["pin"], "123456");
        for line in [
            r#"{"method":"operation"}"#,
            r#"{"method":"FORCE_ACTIVATION_CONFIRMED"}"#,
        ] {
            assert_eq!(
                dispatch(&backend, &shutdown, line)["status"],
                "UNSUPPORTED_OPERATION"
            );
        }
        for line in ["{", "[]", r#"{"method":7}"#] {
            assert_eq!(
                dispatch(&backend, &shutdown, line)["status"],
                "INVALID_REQUEST"
            );
        }
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(
            dispatch(&backend, &shutdown, r#"{"method":"daemonInfo"}"#)["status"],
            "OK"
        );
        assert!(!shutdown.load(Ordering::SeqCst));
        assert_eq!(
            dispatch(&backend, &shutdown, r#"{"method":"shutdown"}"#)["status"],
            "QUEUED"
        );
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn serve_round_trips_over_socket_and_drops_backend_on_shutdown() {
        let path = socket_path();
        let listener = bind(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(bind(&path).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        let (mock, calls, dropped) = mock();
        let server = thread::spawn(move || serve(listener, mock, Duration::from_millis(10)));
        let snapshot = ipc::call(&path, &json!({"method":"snapshot"})).unwrap();
        assert_eq!(snapshot["phase"], "IDLE");
        let pair = ipc::call(&path, &json!({"method":"pair"})).unwrap();
        assert_eq!(pair["status"], "QUEUED");
        thread::sleep(Duration::from_millis(50));
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c["method"] == "snapshot")
                .count()
                > 1
        );
        ipc::call(&path, &json!({"method":"shutdown"})).unwrap();
        server.join().unwrap().unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bind_replaces_stale_socket_but_not_regular_files() {
        let path = socket_path();
        drop(bind(&path).unwrap());
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        drop(bind(&path).unwrap());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"not a socket").unwrap();
        assert_eq!(
            bind(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn oversized_and_unterminated_requests_are_rejected() {
        let path = socket_path();
        let listener = bind(&path).unwrap();
        let (mock, calls, _) = mock();
        let server = thread::spawn(move || serve(listener, mock, Duration::from_secs(60)));
        let send = |bytes: &[u8]| {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(bytes).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            serde_json::from_str::<Value>(&response).unwrap()
        };
        let big = format!(
            "{{\"method\":\"pair\",\"x\":\"{}\"}}\n",
            "a".repeat(MAX_REQUEST)
        );
        assert_eq!(send(big.as_bytes())["status"], "INVALID_REQUEST");
        assert_eq!(send(br#"{"method":"pair"}"#)["status"], "INVALID_REQUEST");
        assert!(calls.lock().unwrap().is_empty());
        ipc::call(&path, &json!({"method":"shutdown"})).unwrap();
        server.join().unwrap().unwrap();
        let _ = fs::remove_file(&path);
    }
}
