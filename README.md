# watchctl

A command-line front end for the Linux desktop backend of
[LordixDemon/apple_watch_connector](https://github.com/LordixDemon/apple_watch_connector).
It drives the same `LinuxBackend` JSON commands the Flutter companion uses:
adapter selection, discovery, pairing with the six-digit code, activation,
owner setup confirmation, reconnect and status.

> Status: M1 (Tier A). Builds and unit-tested; **not yet verified against a
> real Apple Watch**. Operational commands (`ping`, `notify`, settings, ...)
> are Tier B and not implemented yet.

## How it works

```
watchctl <cmd> ──unix socket──> watchctld ──> LinuxBackend ──> java LinuxProtocolHost ──> watch-linux-hci ──> BLE adapter ──> Watch
```

`LinuxBackend` stops the Java protocol worker when it is dropped, so a session
cannot live inside a one-shot CLI process. `watchctld` owns one backend for its
whole lifetime; each `watchctl` call sends one JSON request line and gets one
JSON response line back. The daemon polls `snapshot` every 250 ms (like the
GUI) so the backend keeps reaping its worker and resumes after setup
confirmation.

The backend is a pinned git dependency (`rev = "6485f975"`); no upstream code is
copied into this repository.

## Requirements

The apple_watch_connector Linux runtime must be installed, as described in its
README:

- JDK 17+ on `PATH`
- the protocol runtime directory containing `watch-linux-protocol.jar`
- the raw HCI broker `watch-linux-hci` with
  `cap_net_admin,cap_net_raw=ep` (default
  `/usr/local/libexec/watch-companion/watch-linux-hci`)
- BlueZ (`busctl`) for the adapter inventory, and a BLE adapter the backend can
  take over exclusively while a session runs

Paths are resolved exactly like `LinuxBackend::new`:

| Variable | Default |
|---|---|
| `WATCH_LINUX_PROTOCOL_DIR` | `<dir of watchctld>/lib/protocol` |
| `WATCH_LINUX_HCI_HELPER` | `/usr/local/libexec/watch-companion/watch-linux-hci` |
| `XDG_STATE_HOME` | `~/.local/state` (state lives in `watch-companion/`, shared with the GUI) |
| `WATCHCTL_SOCKET` | `$XDG_RUNTIME_DIR/watchctl.sock` |

Do not run the GUI and `watchctld` at the same time; both would try to own the
adapter.

## Build

```sh
cargo build --release   # rust-toolchain.toml pins Rust 1.88 (edition 2024)
```

## Usage

```sh
WATCH_LINUX_PROTOCOL_DIR=/path/to/lib/protocol ./target/release/watchctld &
watchctl doctor                     # check the installation
watchctl adapters [--refresh]       # list adapters (* = selected)
watchctl adapters select AA:BB:CC:DD:EE:FF
watchctl scan [--timeout 20] [--keep]
watchctl pair [<device-id>] [--pin 123456] [--timeout 300]
watchctl activate retry             # after ACTIVATION_REJECTED
watchctl setup confirm              # owner confirmation, then reconnect
watchctl connect [--no-wait]        # reconnect to the saved pair
watchctl status [--watch]
watchctl disconnect
watchctl log [-f] [-n 50]
watchctl daemon status|stop
```

`pair` picks the Watch automatically when exactly one is discovered and prompts
for the code on the terminal when `--pin` is not given.

### JSON output

`--json` prints one JSON object per result. Streaming commands (`scan`, `pair`,
`connect`, `status --watch`) print one event per line:

```json
{"type":"phase","phase":"PIN_REQUIRED","ts":1760000000000}
{"type":"device","device":{"id":"…","productType":"Watch7,5","watchOs":"26.2","rssi":-51},"ts":…}
{"type":"error","error":"…","ts":…}
{"type":"hint","message":"…","ts":…}
```

`status --json` passes the backend snapshot through unchanged (`apiVersion: 1`).
Errors in JSON mode are `{"status":"<STATUS>","error":"<message>"}`.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | ok / queued / applied |
| 2 | invalid request (bad PIN format, unknown device, nothing to retry/confirm, no saved pair) |
| 3 | busy (a session is already running) |
| 4 | unavailable (daemon not running, backend not installed, adapter missing) |
| 5 | failed or rejected by the Watch |
| 6 | timeout |

## Tests

```sh
cargo test                 # unit tests (dispatch allowlist, socket server, formatting, exit codes)
tests/fake_host/run.sh     # end-to-end: watchctl + watchctld + the real LinuxBackend
```

`tests/fake_host` puts a fake `java` (protocol host), a fake `busctl` (BlueZ) and an
empty HCI broker on `PATH`. The real backend then runs scan, a wrong-PIN pair
attempt, a successful pair to `OPERATIONAL`, busy rejection, disconnect, reconnect
and daemon shutdown. This tests the CLI and backend wiring only. It is not a
substitute for a real Watch.

## Security

- The socket is created with mode `0600`, the daemon only answers peers with
  its own uid (`SO_PEERCRED`), and the client refuses sockets owned by another
  user.
- Only the backend methods listed in `daemon::BACKEND_METHODS` are forwarded;
  the protocol host's research/"force" commands are not reachable.
- Requests (including the PIN) are never logged.

## License

No license has been chosen for this repository yet. The upstream project has no
license file, so redistribute builds that include it only with the upstream
author's permission.
