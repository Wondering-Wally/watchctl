#!/bin/bash
# End-to-end check of watchctl + watchctld + the real LinuxBackend against a
# fake protocol host, fake BlueZ and an empty HCI broker. No hardware needed.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
work="$(mktemp -d)"
trap 'kill "$daemon" 2>/dev/null; rm -rf "$work"' EXIT
mkdir -p -m700 "$work/run" "$work/protocol"
touch "$work/protocol/watch-linux-protocol.jar" "$work/hci" && chmod +x "$work/hci"
export PATH="$here/bin:$PATH" XDG_RUNTIME_DIR="$work/run" XDG_STATE_HOME="$work/state"
export WATCH_LINUX_PROTOCOL_DIR="$work/protocol" WATCH_LINUX_HCI_HELPER="$work/hci" FAKE_PIN_FILE="$work/pin"
cargo build --quiet --manifest-path "$root/Cargo.toml" || exit 1
bin="$root/target/debug"
"$bin/watchctld" 2>"$work/daemon.err" & daemon=$!
for _ in $(seq 50); do [ -S "$work/run/watchctl.sock" ] && break; sleep 0.1; done
fail=0
check() { # name expected-exit command...
  local name=$1 want=$2; shift 2
  out="$("$@" 2>&1)"; got=$?
  if [ "$got" = "$want" ]; then echo "PASS $name"; else echo "FAIL $name (exit $got, want $want)"; echo "$out" | sed 's/^/    /'; fail=1; fi
}
check "adapters lists fake adapter" 0 sh -c "'$bin/watchctl' adapters | grep -q 'hci0  aa:bb:cc:dd:ee:01'"
check "scan finds the Watch" 0 sh -c "'$bin/watchctl' scan --timeout 2 | grep -q 'device 3f2c0a1e'"
echo 654321 > "$work/pin"
check "wrong pin is rejected" 5 "$bin/watchctl" pair --pin 123456 --timeout 20
rm "$work/pin"
check "disconnect after failure" 0 "$bin/watchctl" disconnect
check "pair reaches OPERATIONAL" 0 sh -c "'$bin/watchctl' --json pair --pin 123456 --timeout 30 | tail -1 | grep -q '\"outcome\":\"OPERATIONAL\"'"
check "status is operational" 0 sh -c "'$bin/watchctl' status | grep -q 'phase:      OPERATIONAL'"
check "second pair is busy" 3 "$bin/watchctl" pair --pin 123456
check "disconnect" 0 "$bin/watchctl" disconnect
check "connect resumes" 0 "$bin/watchctl" connect --timeout 20
check "daemon stop" 0 "$bin/watchctl" daemon stop
wait "$daemon"
check "daemon gone" 4 "$bin/watchctl" status
exit $fail
