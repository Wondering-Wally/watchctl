use clap::Parser;
use std::{path::PathBuf, process::ExitCode, sync::atomic::Ordering, time::Duration};
use watchctl::{daemon, ipc};

/// Holds one apple_watch_connector LinuxBackend session for the lifetime of the
/// process and serves `watchctl` requests over a user-only Unix socket.
#[derive(Parser)]
#[command(name = "watchctld", version)]
struct Args {
    /// Socket path (default: $XDG_RUNTIME_DIR/watchctl.sock)
    #[arg(long, env = "WATCHCTL_SOCKET")]
    socket: Option<PathBuf>,
}

extern "C" fn on_signal(_: libc::c_int) {
    daemon::TERMINATE.store(true, Ordering::SeqCst);
}

fn main() -> ExitCode {
    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(ipc::default_socket);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    let listener = match daemon::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("watchctld: {}: {error}", socket.display());
            return ExitCode::from(watchctl::exit::UNAVAILABLE);
        }
    };
    eprintln!("watchctld: listening on {}", socket.display());
    let result = daemon::serve(
        listener,
        watch_transport_linux::LinuxBackend::new(),
        Duration::from_millis(250),
    );
    let _ = std::fs::remove_file(&socket);
    match result {
        Ok(()) => {
            eprintln!("watchctld: stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("watchctld: {error}");
            ExitCode::FAILURE
        }
    }
}
