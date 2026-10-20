#!/bin/bash
# Real multiseatd + seat-launch + patched libseat inherited-fd integration.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
libseat_repo=${LIBSEAT_REPO:-"$repo/../libseat"}
for command in cargo cc dbus-daemon busctl meson ninja sudo; do
    command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 77; }
done
sudo -n true || { echo "passwordless sudo is required" >&2; exit 77; }
id nobody >/dev/null 2>&1 || { echo "user nobody is required" >&2; exit 77; }
cargo build --manifest-path "$repo/Cargo.toml" --workspace --bins

work=$(mktemp -d /tmp/multiseat-root-e2e.XXXXXX)
chmod 755 "$work"
meson setup "$work/libseat-build" "$libseat_repo" \
    -Dserver=disabled -Dexamples=disabled -Dman-pages=disabled \
    -Dlibseat-logind=disabled -Dlibseat-builtin=disabled >/dev/null
ninja -C "$work/libseat-build" >/dev/null
cat >"$work/client.c" <<'EOF'
#include <libseat.h>
#include <stdlib.h>
static void enable(struct libseat *seat, void *data) { (void)seat; *(int *)data = 1; }
static void disable(struct libseat *seat, void *data) { (void)data; libseat_disable_seat(seat); }
int main(void) {
    int enabled = 0;
    const struct libseat_seat_listener listener = { .enable_seat = enable, .disable_seat = disable };
    struct libseat *seat = libseat_open_seat(&listener, &enabled);
    if (seat == NULL || getenv("SEATD_FD") != NULL) return 1;
    while (!enabled) if (libseat_dispatch(seat, 2000) < 0) return 2;
    return libseat_close_seat(seat) < 0 ? 3 : 0;
}
EOF
cc "$work/client.c" -I"$libseat_repo/include" -L"$work/libseat-build" \
    -Wl,-rpath,"$work/libseat-build" -lseat -o "$work/client"
cat >"$work/bus.conf" <<EOF
<busconfig><type>system</type><listen>unix:path=$work/bus</listen><auth>EXTERNAL</auth><policy context="default"><allow user="*"/><allow own="*"/><allow send_type="method_call"/><allow send_type="method_return"/><allow send_type="error"/><allow send_type="signal"/><allow receive_type="method_call"/><allow receive_type="method_return"/><allow receive_type="error"/><allow receive_type="signal"/></policy></busconfig>
EOF
bus_pid=$(dbus-daemon --config-file="$work/bus.conf" --fork --print-pid)
address="unix:path=$work/bus"
daemon_wrapper=
cleanup() {
    status=$?
    trap - EXIT
    set +e
    [ -n "$daemon_wrapper" ] && sudo kill "$daemon_wrapper" 2>/dev/null
    wait 2>/dev/null
    kill "$bus_pid" 2>/dev/null
    if [ "$status" -ne 0 ] && [ -f "$work/multiseatd.log" ]; then sudo tail -100 "$work/multiseatd.log" >&2; fi
    sudo rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
sudo -E env DBUS_SYSTEM_BUS_ADDRESS="$address" "$repo/target/debug/multiseatd" \
    --bus system --runtime-dir "$work/run" --test-seat seat1 >"$work/multiseatd.log" 2>&1 & daemon_wrapper=$!
for _ in $(seq 1 100); do
    busctl --address="$address" list 2>/dev/null | grep -q org.decoupling.multiseatd1 && break
    sleep .05
done
result=$(busctl --address="$address" call org.decoupling.multiseatd1 /org/decoupling/multiseatd1 org.decoupling.multiseatd1.Manager ListSeats)
echo "$result" | grep -q 'seat1'
sudo "$repo/target/debug/seat-launch" --seat seat1 --user nobody --runtime-dir "$work/run" -- "$work/client"
echo "multiseatd/seat-launch/libseat inherited-fd end-to-end test passed"
