use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    process::{Command, ExitCode},
    thread,
    time::{Duration, Instant},
};
use watchctl::{
    exit,
    format::{self, Tracker},
    ipc,
    paths::{self, Paths},
};

const POLL: Duration = Duration::from_millis(250);

/// Command-line client for watchctld, which owns the apple_watch_connector
/// Linux backend session.
#[derive(Parser)]
#[command(name = "watchctl", version)]
struct Cli {
    /// Daemon socket (default: $XDG_RUNTIME_DIR/watchctl.sock)
    #[arg(long, global = true, env = "WATCHCTL_SOCKET")]
    socket: Option<PathBuf>,
    /// JSON output: one object per result, one line per event while streaming
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check Java, the protocol runtime, the HCI broker, adapters and the daemon
    Doctor,
    /// List Bluetooth adapters, or select the one the backend takes over
    Adapters {
        /// Re-read the adapter inventory from BlueZ first
        #[arg(long)]
        refresh: bool,
        #[command(subcommand)]
        action: Option<AdapterCmd>,
    },
    /// Discover nearby Watches
    Scan {
        /// Seconds to listen
        #[arg(long, default_value_t = 20)]
        timeout: u64,
        /// Leave discovery running afterwards (so `pair` can reuse it)
        #[arg(long)]
        keep: bool,
    },
    /// Discover, connect, submit the six-digit code and wait for activation
    Pair {
        /// Device id from `watchctl scan` (optional when only one Watch is found)
        device: Option<String>,
        /// Six-digit code shown on the Watch (prompted for when omitted)
        #[arg(long)]
        pin: Option<String>,
        /// Seconds to wait for the whole flow
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Activation controls
    Activate {
        #[command(subcommand)]
        action: ActivateCmd,
    },
    /// Owner setup confirmation
    Setup {
        #[command(subcommand)]
        action: SetupCmd,
    },
    /// Reconnect to the saved, activated pair
    Connect {
        /// Return as soon as the reconnect is queued
        #[arg(long)]
        no_wait: bool,
        /// Seconds to wait for OPERATIONAL
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Show the backend snapshot
    Status {
        /// Keep polling and print changes
        #[arg(long)]
        watch: bool,
    },
    /// Stop the session and release the adapter
    Disconnect,
    /// Print the protocol log
    Log {
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
        /// Number of trailing lines
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Daemon controls
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum AdapterCmd {
    /// Select an adapter by Bluetooth address
    Select { address: String },
}
#[derive(Subcommand)]
enum ActivateCmd {
    /// Retry a pending or rejected activation
    Retry,
}
#[derive(Subcommand)]
enum SetupCmd {
    /// Confirm setup as the owner and reconnect
    Confirm,
}
#[derive(Subcommand)]
enum DaemonCmd {
    /// Show daemon version, pid and paths
    Status,
    /// Stop the session and exit the daemon
    Stop,
}

/// A finished command that did not succeed; carries the exit code.
struct Fail(u8);
type Res = Result<(), Fail>;

fn out(line: &str) {
    let mut stdout = io::stdout().lock();
    if writeln!(stdout, "{line}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        std::process::exit(0);
    }
}

fn fail(json: bool, status: &str, message: &str) -> Fail {
    if json {
        out(&json!({"status":status,"error":message}).to_string());
    }
    eprintln!("watchctl: {message}");
    Fail(exit::for_status(status))
}

fn session_failed(json: bool, snapshot: &Value) -> Fail {
    let message = snapshot["error"].as_str().unwrap_or("session failed");
    let code = fail(json, "FAILED", message);
    if !json {
        eprintln!("watchctl: details: `watchctl log`");
    }
    code
}

fn phase(snapshot: &Value) -> &str {
    snapshot["phase"].as_str().unwrap_or("")
}

fn setup_confirm_required(snapshot: &Value) -> bool {
    snapshot["pairId"].is_string()
        && snapshot["activationConfirmed"] == true
        && snapshot["operationalEligible"] != true
}

struct Ctx {
    socket: PathBuf,
    json: bool,
    tracker: Tracker,
}

impl Ctx {
    fn call(&self, request: Value) -> Result<Value, Fail> {
        ipc::call(&self.socket, &request)
            .map_err(|error| fail(self.json, "DAEMON_UNREACHABLE", &error.to_string()))
    }

    fn request(&self, method: &str, extra: Value) -> Result<Value, Fail> {
        let mut request = json!({"method":method});
        if let Value::Object(extra) = extra {
            request.as_object_mut().unwrap().extend(extra);
        }
        let response = self.call(request)?;
        if let Some(status) = response["status"].as_str()
            && exit::for_status(status) != exit::OK
        {
            let message = response["reason"]
                .as_str()
                .unwrap_or(exit::describe(status));
            return Err(fail(self.json, status, message));
        }
        Ok(response)
    }

    fn snapshot(&self) -> Result<Value, Fail> {
        self.call(json!({"method":"snapshot"}))
    }

    fn emit(&self, value: &Value, human: impl FnOnce() -> String) {
        if self.json {
            out(&value.to_string());
        } else {
            out(&human());
        }
    }

    fn event(&self, mut event: Value) {
        event["ts"] = json!(format::now_ms());
        if self.json {
            out(&event.to_string());
        } else {
            out(&format::event_human(&event));
        }
    }

    fn hint(&self, message: &str) {
        self.event(json!({"type":"hint","message":message}));
    }

    fn poll(&mut self) -> Result<Value, Fail> {
        let snapshot = self.snapshot()?;
        for event in self.tracker.events(&snapshot) {
            self.event(event);
        }
        Ok(snapshot)
    }

    /// Polls until `check` decides, printing change events along the way.
    fn wait<T>(
        &mut self,
        deadline: Instant,
        mut check: impl FnMut(&Value) -> Option<Result<T, Fail>>,
    ) -> Result<T, Fail> {
        loop {
            let snapshot = self.poll()?;
            if let Some(result) = check(&snapshot) {
                return result;
            }
            if Instant::now() >= deadline {
                return Err(fail(
                    self.json,
                    "TIMEOUT",
                    "timed out waiting for the Watch",
                ));
            }
            thread::sleep(POLL);
        }
    }

    /// Starts discovery unless it is already running. Returns whether it started.
    fn start_discovery(&mut self, method: &str) -> Result<bool, Fail> {
        let snapshot = self.snapshot()?;
        match phase(&snapshot) {
            "IDLE" | "FAILED" => {
                self.request(method, json!({}))?;
                Ok(true)
            }
            "DISCOVERING" => Ok(false),
            other => Err(fail(
                self.json,
                "BUSY",
                &format!("session is busy in phase {other}; stop it with `watchctl disconnect`"),
            )),
        }
    }
}

fn adapters(ctx: &Ctx, refresh: bool, action: Option<AdapterCmd>) -> Res {
    if let Some(AdapterCmd::Select { address }) = action {
        let response = ctx.request("selectAdapter", json!({"id":address}))?;
        ctx.emit(&response, || format!("selected adapter {address}"));
        return Ok(());
    }
    if refresh {
        ctx.request("refreshAdapters", json!({}))?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = ctx.snapshot()?;
        let loading = snapshot["adapterInventoryRefreshing"] == true
            || snapshot["adapterSelectionStatus"] == "LOADING";
        if !loading || Instant::now() >= deadline {
            break snapshot;
        }
        thread::sleep(POLL);
    };
    let keys = [
        "bluetoothAdapters",
        "selectedAdapterId",
        "adapterSelectionStatus",
        "adapterSelectionBusy",
        "adapterSelectionError",
    ];
    let summary: serde_json::Map<String, Value> = keys
        .iter()
        .map(|k| (k.to_string(), snapshot[*k].clone()))
        .collect();
    ctx.emit(&Value::Object(summary), || {
        format::adapters_human(&snapshot)
    });
    Ok(())
}

fn scan(ctx: &mut Ctx, timeout: u64, keep: bool) -> Res {
    let started = ctx.start_discovery("scan")?;
    let json = ctx.json;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let last = loop {
        let snapshot = ctx.poll()?;
        if phase(&snapshot) == "FAILED" {
            return Err(session_failed(json, &snapshot));
        }
        if Instant::now() >= deadline {
            break snapshot;
        }
        thread::sleep(POLL);
    };
    if started && !keep {
        ctx.request("stop", json!({}))?;
    }
    let devices = last["devices"].as_array().cloned().unwrap_or_default();
    ctx.emit(&json!({"type":"summary","devices":devices}), || {
        format!("found {} Watch(es)", devices.len())
    });
    Ok(())
}

fn valid_pin(pin: &str) -> bool {
    pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit())
}

fn prompt_pin(json: bool) -> Result<String, Fail> {
    eprint!("Enter the six-digit code shown on the Watch: ");
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 && valid_pin(line.trim()) => Ok(line.trim().to_string()),
        _ => Err(fail(json, "INVALID_REQUEST", "expected a six-digit code")),
    }
}

const AFTER_PIN: &[&str] = &[
    "PIN_AUTHENTICATION",
    "CONFIGURING",
    "ACTIVATING",
    "ACTIVATION_INPUT",
    "ACTIVATED",
    "SYNCING",
    "WAITING_FOR_WATCH",
    "VERIFYING_RECONNECT",
    "IDS",
    "OPERATIONAL",
];

fn pair(ctx: &mut Ctx, device: Option<String>, pin: Option<String>, timeout: u64) -> Res {
    let json = ctx.json;
    if pin.as_deref().is_some_and(|p| !valid_pin(p)) {
        return Err(fail(json, "INVALID_REQUEST", "--pin must be six digits"));
    }
    let deadline = Instant::now() + Duration::from_secs(timeout);
    ctx.start_discovery("pair")?;
    let id = ctx.wait(deadline, |snapshot| {
        if phase(snapshot) == "FAILED" {
            return Some(Err(session_failed(json, snapshot)));
        }
        let ids: Vec<&str> = snapshot["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["id"].as_str())
            .collect();
        match &device {
            Some(wanted) => ids.contains(&wanted.as_str()).then(|| Ok(wanted.clone())),
            None => match ids.as_slice() {
                [] => None,
                [only] => Some(Ok(only.to_string())),
                _ => Some(Err(fail(
                    json,
                    "AMBIGUOUS_DEVICE",
                    "several Watches found; rerun with `watchctl pair <id>`",
                ))),
            },
        }
    })?;
    ctx.request("connect", json!({"id":id}))?;
    let reached = ctx.wait(deadline, |snapshot| match phase(snapshot) {
        "PIN_REQUIRED" => Some(Ok(true)),
        "FAILED" => Some(Err(session_failed(json, snapshot))),
        p if AFTER_PIN.contains(&p) => Some(Ok(false)),
        _ => None,
    })?;
    if reached {
        let pin = match pin {
            Some(pin) => pin,
            None => prompt_pin(json)?,
        };
        ctx.request("submitPin", json!({"pin":pin}))?;
    }
    let outcome = ctx.wait(deadline, |snapshot| match phase(snapshot) {
        "OPERATIONAL" => Some(Ok("OPERATIONAL")),
        "FAILED" => Some(Err(session_failed(json, snapshot))),
        "PIN_REQUIRED" => Some(Err(fail(
            json,
            "PIN_REJECTED",
            "the Watch asked for the code again; check it and run `watchctl pair` again",
        ))),
        "ACTIVATION_REJECTED" => Some(Err(fail(
            json,
            "ACTIVATION_REJECTED",
            "the Watch rejected activation; retry with `watchctl activate retry`",
        ))),
        "IDS" if setup_confirm_required(snapshot) => Some(Ok("SETUP_CONFIRM_REQUIRED")),
        _ => None,
    })?;
    if outcome == "SETUP_CONFIRM_REQUIRED" {
        ctx.hint("activated; confirm setup with `watchctl setup confirm`");
    }
    ctx.emit(
        &json!({"status":"OK","outcome":outcome,"device":id}),
        || match outcome {
            "OPERATIONAL" => "paired: Watch is operational".into(),
            _ => "paired: waiting for setup confirmation".into(),
        },
    );
    Ok(())
}

fn wait_operational(ctx: &mut Ctx, timeout: u64) -> Res {
    let json = ctx.json;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    ctx.wait(deadline, |snapshot| match phase(snapshot) {
        "OPERATIONAL" => Some(Ok(())),
        "FAILED" => Some(Err(session_failed(json, snapshot))),
        "ACTIVATION_REJECTED" => Some(Err(fail(
            json,
            "ACTIVATION_REJECTED",
            "the Watch rejected activation; retry with `watchctl activate retry`",
        ))),
        _ => None,
    })?;
    ctx.emit(&json!({"status":"OK","phase":"OPERATIONAL"}), || {
        "connected: Watch is operational".into()
    });
    Ok(())
}

fn connect(ctx: &mut Ctx, no_wait: bool, timeout: u64) -> Res {
    let snapshot = ctx.snapshot()?;
    if !snapshot["pairId"].is_string() {
        return Err(fail(ctx.json, "NO_PAIR", exit::describe("NO_PAIR")));
    }
    let response = ctx.request("resume", json!({}))?;
    if no_wait {
        ctx.emit(&response, || "reconnect queued".into());
        return Ok(());
    }
    wait_operational(ctx, timeout)
}

fn setup_confirm(ctx: &mut Ctx) -> Res {
    ctx.request("confirmSetup", json!({}))?;
    if !ctx.json {
        out("setup confirmed; reconnecting");
    }
    wait_operational(ctx, 120)
}

fn disconnect(ctx: &mut Ctx) -> Res {
    let response = ctx.request("stop", json!({}))?;
    if response["status"] == "IDLE" {
        ctx.emit(&response, || "already idle".into());
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    ctx.wait(deadline, |s| {
        ["IDLE", "FAILED"].contains(&phase(s)).then_some(Ok(()))
    })?;
    ctx.emit(&json!({"status":"OK","phase":"IDLE"}), || {
        "disconnected".into()
    });
    Ok(())
}

fn status(ctx: &mut Ctx, watch: bool) -> Res {
    if !watch {
        let snapshot = ctx.snapshot()?;
        ctx.emit(&snapshot, || format::snapshot_human(&snapshot));
        return Ok(());
    }
    loop {
        ctx.poll()?;
        thread::sleep(POLL);
    }
}

fn tail(path: &PathBuf, lines: usize) -> io::Result<(Vec<String>, u64)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(512 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        all.remove(0);
    }
    let skip = all.len().saturating_sub(lines);
    Ok((all[skip..].iter().map(|s| s.to_string()).collect(), length))
}

fn log(ctx: &Ctx, follow: bool, lines: usize) -> Res {
    let paths = ctx
        .call(json!({"method":"daemonInfo"}))
        .ok()
        .and_then(|info| Paths::from_json(&info))
        .unwrap_or_else(Paths::resolve);
    let path = paths.log();
    let print = |line: &str| {
        if ctx.json {
            out(&json!({"type":"log","line":line}).to_string());
        } else {
            out(line);
        }
    };
    let (initial, mut position) = tail(&path, lines)
        .map_err(|e| fail(ctx.json, "UNAVAILABLE", &format!("{}: {e}", path.display())))?;
    initial.iter().for_each(|l| print(l));
    if !follow {
        return Ok(());
    }
    loop {
        thread::sleep(POLL);
        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let length = file.metadata().map(|m| m.len()).unwrap_or(0);
        if length < position {
            position = 0;
        }
        if length == position || file.seek(SeekFrom::Start(position)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        position += complete as u64;
        String::from_utf8_lossy(&bytes[..complete])
            .lines()
            .for_each(print);
    }
}

fn doctor(ctx: &Ctx) -> Res {
    let mut checks = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String| {
        checks.push(json!({"check":name,"ok":ok,"detail":detail}));
    };
    let info = ctx.call(json!({"method":"daemonInfo"}));
    let paths = info
        .as_ref()
        .ok()
        .and_then(Paths::from_json)
        .unwrap_or_else(Paths::resolve);
    match &info {
        Ok(info) => add(
            "daemon",
            true,
            format!(
                "watchctld {} (pid {})",
                info["daemonVersion"].as_str().unwrap_or("?"),
                info["pid"]
            ),
        ),
        Err(_) => add(
            "daemon",
            false,
            format!(
                "not reachable at {}; start `watchctld`",
                ctx.socket.display()
            ),
        ),
    }
    let java = Command::new("java").arg("-version").output();
    match java {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stderr);
            add(
                "java",
                true,
                version.lines().next().unwrap_or("").to_string(),
            );
        }
        Ok(output) => add(
            "java",
            false,
            format!("`java -version` exited with {}", output.status),
        ),
        Err(error) => add(
            "java",
            false,
            format!("java not on PATH ({error}); JDK 17+ is required"),
        ),
    }
    let jar = paths.protocol.join(paths::PROTOCOL_JAR);
    add(
        "protocol runtime",
        jar.is_file(),
        if jar.is_file() {
            jar.display().to_string()
        } else {
            format!(
                "{} missing; set WATCH_LINUX_PROTOCOL_DIR for watchctld",
                jar.display()
            )
        },
    );
    add(
        "hci broker",
        paths.helper.is_file(),
        if paths.helper.is_file() {
            paths.helper.display().to_string()
        } else {
            format!(
                "{} missing; set WATCH_LINUX_HCI_HELPER for watchctld",
                paths.helper.display()
            )
        },
    );
    if paths.helper.is_file() {
        match Command::new("getcap").arg(&paths.helper).output() {
            Ok(output) => {
                let caps = String::from_utf8_lossy(&output.stdout).to_string();
                let ok = caps.contains("cap_net_admin") && caps.contains("cap_net_raw");
                add(
                    "broker capabilities",
                    ok,
                    if ok {
                        caps.trim().to_string()
                    } else {
                        format!(
                            "needs `sudo setcap cap_net_admin,cap_net_raw=ep {}`",
                            paths.helper.display()
                        )
                    },
                );
            }
            Err(error) => add(
                "broker capabilities",
                false,
                format!("getcap unavailable ({error}); install libcap2-bin"),
            ),
        }
    }
    let controllers: Vec<String> = std::fs::read_dir("/sys/class/bluetooth")
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|name| name.starts_with("hci"))
                .collect()
        })
        .unwrap_or_default();
    add(
        "bluetooth controllers",
        !controllers.is_empty(),
        if controllers.is_empty() {
            "none in /sys/class/bluetooth".into()
        } else {
            controllers.join(", ")
        },
    );
    let bluez = Command::new("busctl")
        .args(["--system", "status", "org.bluez"])
        .output();
    add(
        "bluez",
        matches!(&bluez, Ok(o) if o.status.success()),
        match &bluez {
            Ok(o) if o.status.success() => "org.bluez reachable on the system bus".into(),
            Ok(_) => "org.bluez not running (adapter inventory needs BlueZ)".into(),
            Err(error) => format!("busctl unavailable ({error})"),
        },
    );
    if info.is_ok()
        && let Ok(snapshot) = ctx.snapshot()
    {
        let reason = snapshot["capabilities"]["pairingUnavailableReason"].as_str();
        add(
            "backend",
            reason.is_none(),
            reason.map_or_else(|| format!("phase {}", phase(&snapshot)), str::to_string),
        );
        add(
            "adapter selection",
            snapshot["adapterSelectionStatus"] == "READY",
            format!(
                "{} {}",
                snapshot["adapterSelectionStatus"].as_str().unwrap_or("?"),
                snapshot["selectedAdapterId"].as_str().unwrap_or("")
            )
            .trim_end()
            .to_string(),
        );
    }
    let healthy = checks.iter().all(|c| c["ok"] == true);
    if ctx.json {
        out(&json!({"status":if healthy {"OK"} else {"UNAVAILABLE"},"checks":checks}).to_string());
    } else {
        for check in &checks {
            let mark = if check["ok"] == true { "ok  " } else { "FAIL" };
            out(&format!(
                "{mark}  {:<22} {}",
                check["check"].as_str().unwrap_or(""),
                check["detail"].as_str().unwrap_or("")
            ));
        }
    }
    if healthy {
        Ok(())
    } else {
        Err(Fail(exit::UNAVAILABLE))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut ctx = Ctx {
        socket: cli.socket.unwrap_or_else(ipc::default_socket),
        json: cli.json,
        tracker: Tracker::default(),
    };
    let result = match cli.command {
        Cmd::Doctor => doctor(&ctx),
        Cmd::Adapters { refresh, action } => adapters(&ctx, refresh, action),
        Cmd::Scan { timeout, keep } => scan(&mut ctx, timeout, keep),
        Cmd::Pair {
            device,
            pin,
            timeout,
        } => pair(&mut ctx, device, pin, timeout),
        Cmd::Activate {
            action: ActivateCmd::Retry,
        } => ctx
            .request("retryActivation", json!({}))
            .map(|r| ctx.emit(&r, || "activation retry queued".into())),
        Cmd::Setup {
            action: SetupCmd::Confirm,
        } => setup_confirm(&mut ctx),
        Cmd::Connect { no_wait, timeout } => connect(&mut ctx, no_wait, timeout),
        Cmd::Status { watch } => status(&mut ctx, watch),
        Cmd::Disconnect => disconnect(&mut ctx),
        Cmd::Log { follow, lines } => log(&ctx, follow, lines),
        Cmd::Daemon {
            action: DaemonCmd::Status,
        } => ctx.request("daemonInfo", json!({})).map(|info| {
            ctx.emit(&info, || {
                format!(
                    "watchctld {} pid {}\nprotocol: {}\nbroker:   {}\nstate:    {}",
                    info["daemonVersion"].as_str().unwrap_or("?"),
                    info["pid"],
                    info["protocolDir"].as_str().unwrap_or("?"),
                    info["hciHelper"].as_str().unwrap_or("?"),
                    info["stateDir"].as_str().unwrap_or("?"),
                )
            })
        }),
        Cmd::Daemon {
            action: DaemonCmd::Stop,
        } => ctx
            .request("shutdown", json!({}))
            .map(|r| ctx.emit(&r, || "watchctld stopping".into())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Fail(code)) => ExitCode::from(code),
    }
}
