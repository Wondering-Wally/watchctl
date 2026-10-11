#!/bin/bash
# Runs `watchctl identify` against a fake BlueZ on a private D-Bus bus.
# Checks the real D-Bus property types (Address, RSSI, ServiceData a{sv}/ay).
set -u
root="$(cd "$(dirname "$0")/../.." && pwd)"
cargo build --quiet --manifest-path "$root/Cargo.toml" --bins --examples || exit 1
exec dbus-run-session -- bash -c '
  root="$1"; work="$(mktemp -d)"; trap "kill \$fake 2>/dev/null; rm -rf \$work" EXIT
  "$root/target/debug/examples/fake_bluez" > "$work/fake.out" & fake=$!
  for _ in $(seq 50); do grep -q ready "$work/fake.out" 2>/dev/null && break; sleep 0.1; done
  export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" XDG_STATE_HOME="$work/state"
  fail=0
  human="$("$root/target/debug/watchctl" identify --timeout 2 2>/dev/null)"; code=$?
  echo "$human" | sed "s/^/    /"
  check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
  check "exit 0 when Watches are seen" "[ $code = 0 ]"
  check "Ultra 2 decoded and pairable" "echo \"\$human\" | grep -q \"Watch7,5  watchOS 26.2.0  pairing v25  rssi -51  11:22:33:44:55:01\""
  check "other model reported with reason" "echo \"\$human\" | grep -q \"not pairable as-is: model Watch6,5 is not Watch7,5\""
  check "non-setup devices ignored" "! echo \"\$human\" | grep -q \"55:0[34]\""
  check "run saved" "ls \$work/state/watch-companion/runs/*/identify.json >/dev/null 2>&1"
  json="$("$root/target/debug/watchctl" --json identify --timeout 2 --no-save 2>/dev/null)"
  check "json summary" "echo \"\$json\" | tail -1 | grep -q \"\\\"count\\\":2\""
  exit $fail
' _ "$root"
