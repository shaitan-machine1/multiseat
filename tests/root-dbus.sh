#!/bin/bash
# Private system-bus contract tests; no live bus, udev or device ACLs touched.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
if (( EUID != 0 )); then
    for tool in cargo sudo dbus-daemon timeout; do
        command -v "$tool" >/dev/null || exit 77
    done
    sudo -n true || exit 77
    cargo build --manifest-path "$repo/Cargo.toml" --workspace --examples
    cargo build --manifest-path "$repo/Cargo.toml" --workspace
    exec sudo -n bash "$0" --root
fi
work=$(mktemp -d /tmp/multiseat-dbus.XXXXXX)
chmod 777 "$work"
pids=()
cleanup() {
    status=$?
    trap - EXIT
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    if (( status != 0 )); then tail -n 100 "$work"/*.log >&2 || true; fi
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
cp "$repo/target/debug/examples/contract-probe" "$work/contract-probe"
chmod 755 "$work/contract-probe"
probe="$work/contract-probe"
daemon_program="$repo/target/debug/multiseatd"
wait_file() {
    for ((i=0; i<200; i++)); do [[ -e "$1" ]] && return; sleep .02; done
    echo "timed out waiting for $1" >&2; return 1
}
start_daemon() {
    "$daemon_program" --test-only --test-seat seat-test >"$work/daemon.log" 2>&1 &
    daemon=$!; pids+=("$daemon")
    for ((i=0; i<200; i++)); do
        if "$probe" ready 2>/dev/null; then return; fi
        kill -0 "$daemon" || return 1
        sleep .02
    done
    return 1
}
for policy in permissive production; do
    include=""
    if [[ $policy == production ]]; then include="<include>$repo/dist/dbus/org.decoupling.multiseatd1.conf</include>"; fi
    cat >"$work/bus.conf" <<EOF
<busconfig>
  <type>system</type><listen>unix:path=$work/bus</listen><auth>EXTERNAL</auth>
  <policy context="default"><allow user="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy>
  <policy user="root"><allow own="*"/></policy>
  $include
</busconfig>
EOF
    dbus-daemon --nofork --config-file="$work/bus.conf" >"$work/bus.log" 2>&1 &
    bus=$!; pids+=("$bus")
    wait_file "$work/bus"
    chmod 777 "$work/bus"
    export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$work/bus"
    start_daemon
    [[ $(find "/proc/$daemon/task" -mindepth 1 -maxdepth 1 -type d | wc -l) == 2 ]]
    timeout 15 "$probe" contract
    timeout 15 sudo -u nobody env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" "$probe" deny
    # The name cannot be replaced by a second daemon.
    if "$daemon_program" --test-only --test-seat seat-test >"$work/second.log" 2>&1; then exit 1; fi
    "$probe" hold "$work/hold" & holder=$!; pids+=("$holder")
    wait_file "$work/hold"
    kill -TERM "$daemon"; wait "$daemon"; wait "$holder"
    rm "$work/hold"
    # Real D-Bus policy restricts broadcast delivery, not only method calls.
    "$probe" watch "$work/root-watch" yes & root_watch=$!; pids+=("$root_watch")
    expected=yes; [[ $policy == production ]] && expected=no
    sudo -u nobody env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" "$probe" watch "$work/user-watch" "$expected" & user_watch=$!; pids+=("$user_watch")
    "$probe" emit "$work/emitter" & emitter=$!; pids+=("$emitter")
    wait_file "$work/root-watch"; wait_file "$work/user-watch"; wait_file "$work/emitter"
    touch "$work/emitter.go"
    wait "$root_watch"; wait "$user_watch"; wait "$emitter"
    rm "$work/root-watch" "$work/user-watch" "$work/emitter" "$work/emitter.go"
    start_daemon
    "$probe" hold "$work/hold" & holder=$!; pids+=("$holder")
    wait_file "$work/hold"
    kill "$bus"; wait "$bus"
    if wait "$daemon"; then echo "daemon must fail on bus loss" >&2; exit 1; fi
    wait "$holder"
    rm -f "$work/hold" "$work/bus"
done
echo "Private D-Bus authorization, FD lifetime, signals, owner loss and thread-count tests passed"
