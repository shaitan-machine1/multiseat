#!/bin/bash
# Embedded daemon + D-Bus seat-launch + real patched libseat. No live devices.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# shellcheck source=tests/libseat-source.sh
. "$repo/tests/libseat-source.sh"
for tool in cargo cc dbus-daemon busctl meson ninja sudo timeout; do
    command -v "$tool" >/dev/null || { echo "missing command: $tool" >&2; exit 77; }
done
sudo -n true || exit 77
id nobody >/dev/null 2>&1 || exit 77
set +e
libseat_repo=$(resolve_libseat_source)
resolved=$?
set -e
if (( resolved == 77 )); then
    echo "SKIP: could not fetch the patched libseat source" >&2
    exit 77
fi
(( resolved == 0 )) || { echo "invalid LIBSEAT_REPO" >&2; exit 1; }
cargo build --manifest-path "$repo/Cargo.toml" --workspace --bins --features multiseatd/test-support
work=$(mktemp -d /tmp/multiseat-root-e2e.XXXXXX)
chmod 755 "$work"
bus_pid= daemon_pid=
cleanup() {
    status=$?
    trap - EXIT
    [[ -z $daemon_pid ]] || sudo kill "$daemon_pid" 2>/dev/null || true
    [[ -z $bus_pid ]] || kill "$bus_pid" 2>/dev/null || true
    wait 2>/dev/null || true
    if (( status != 0 )); then tail -n 100 "$work/daemon.log" >&2 || true; fi
    sudo rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
meson setup "$work/libseat-build" "$libseat_repo" \
    -Dserver=disabled -Dexamples=disabled -Dman-pages=disabled \
    -Dlibseat-logind=disabled -Dlibseat-builtin=disabled >/dev/null
ninja -C "$work/libseat-build" >/dev/null
cat >"$work/client.c" <<'EOF'
#include <libseat.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
static void enable(struct libseat *seat, void *data) { (void)seat; *(int *)data = 1; }
static void disable(struct libseat *seat, void *data) { (void)data; libseat_disable_seat(seat); }
int main(void) {
    if (geteuid() == 0) return 10;
    if (!getenv("LIBSEAT_BACKEND") || strcmp(getenv("LIBSEAT_BACKEND"), "seatd")) return 11;
    if (!getenv("XDG_SEAT") || strcmp(getenv("XDG_SEAT"), "seat-test")) return 12;
    int enabled = 0;
    const struct libseat_seat_listener listener = { .enable_seat = enable, .disable_seat = disable };
    struct libseat *seat = libseat_open_seat(&listener, &enabled);
    if (seat == NULL || getenv("SEATD_FD") != NULL) return 1;
    while (!enabled) if (libseat_dispatch(seat, 2000) < 0) return 2;
    if (strcmp(libseat_seat_name(seat), "seat-test") != 0) return 3;
    int fd = -1;
    int device = libseat_open_device(seat, "/dev/null", &fd);
    if (device < 0 || fd < 0) return 4;
    if ((fcntl(fd, F_GETFD) & FD_CLOEXEC) == 0) return 5;
    if (libseat_close_device(seat, device) < 0) return 6;
    close(fd);
    return libseat_close_seat(seat) < 0 ? 7 : 0;
}
EOF
cc "$work/client.c" -I"$libseat_repo/include" -L"$work/libseat-build" \
    -Wl,-rpath,"$work/libseat-build" -lseat -o "$work/client"
cat >"$work/bus.conf" <<EOF
<busconfig><type>system</type><listen>unix:path=$work/bus</listen><auth>EXTERNAL</auth>
<policy context="default"><allow user="*"/><allow send_destination="*"/><allow receive_sender="*"/></policy>
<policy user="root"><allow own="*"/></policy>
<include>$repo/dist/dbus/org.decoupling.multiseatd1.conf</include></busconfig>
EOF
bus_pid=$(dbus-daemon --config-file="$work/bus.conf" --fork --print-pid)
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$work/bus"
sudo env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" \
    sh -c 'echo $$ > "$1"; shift; exec "$@"' _ "$work/daemon.pid" \
    "$repo/target/debug/multiseatd" --test-only --test-seat seat-test --test-device /dev/null \
    --lock-file "$work/daemon.lock" \
    >"$work/daemon.log" 2>&1 & daemon_wrapper=$!
for ((i=0; i<200; i++)); do
    if [[ -f $work/daemon.pid ]]; then daemon_pid=$(<"$work/daemon.pid"); fi
    if sudo env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" busctl call \
        org.decoupling.multiseatd1 /org/decoupling/multiseatd1 org.decoupling.multiseatd1.Manager ListSeats 2>/dev/null | grep -q seat-test; then break; fi
    sleep .02
done
sudo env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" \
    timeout 15 "$repo/target/debug/seat-launch" --seat seat-test --user nobody --fd 57 -- "$work/client"
# Failed exec must also release/revoke the client and close the parent FD.
if sudo env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" \
    "$repo/target/debug/seat-launch" --seat seat-test --user nobody -- /no/such/compositor; then exit 1; fi
inventory=$(sudo env DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SYSTEM_BUS_ADDRESS" busctl call \
    org.decoupling.multiseatd1 /org/decoupling/multiseatd1 org.decoupling.multiseatd1.Manager ListClients)
[[ $inventory == 'a(ssus) 0' ]]
sudo kill -TERM "$daemon_pid"
wait "$daemon_wrapper"
daemon_pid=
echo "Embedded multiseatd/D-Bus seat-launch/patched libseat end-to-end test passed"
