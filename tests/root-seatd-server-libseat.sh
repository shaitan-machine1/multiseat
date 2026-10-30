#!/bin/bash
# Rust seatd-server + patched real libseat inherited-FD compatibility test.
set -euo pipefail
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# shellcheck source=tests/libseat-source.sh
. "$repo/tests/libseat-source.sh"
for command in cargo cc meson ninja sudo; do
    command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 77; }
done
sudo -n true || { echo "passwordless sudo is required" >&2; exit 77; }
set +e
libseat_repo=$(resolve_libseat_source)
resolved=$?
set -e
if (( resolved == 77 )); then
    echo "SKIP: could not fetch the patched libseat source" >&2
    exit 77
fi
(( resolved == 0 )) || { echo "invalid LIBSEAT_REPO" >&2; exit 1; }

work=$(mktemp -d /tmp/seatd-server-libseat.XXXXXX)
cleanup() {
    status=$?
    trap - EXIT
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

meson setup "$work/libseat-build" "$libseat_repo" \
    -Dserver=disabled -Dexamples=disabled -Dman-pages=disabled \
    -Dlibseat-logind=disabled -Dlibseat-builtin=disabled >/dev/null
ninja -C "$work/libseat-build" >/dev/null
cat >"$work/client.c" <<'EOF'
#include <fcntl.h>
#include <libseat.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define FIRST_FD 57

static void enable(struct libseat *seat, void *data) {
    (void)seat;
    (*(int *)data)++;
}

static void disable(struct libseat *seat, void *data) {
    (void)data;
    libseat_disable_seat(seat);
}

static int run_backend(int *enabled) {
    const struct libseat_seat_listener listener = {
        .enable_seat = enable,
        .disable_seat = disable,
    };
    struct libseat *seat = libseat_open_seat(&listener, enabled);
    if (seat == NULL || getenv("SEATD_FD") != NULL) return 1;
    while (*enabled == 0) if (libseat_dispatch(seat, 2000) < 0) return 2;
    if (strcmp(libseat_seat_name(seat), "seat-test") != 0) return 3;
    int fd = -1;
    int device = libseat_open_device(seat, "/dev/null", &fd);
    if (device < 0 || fd < 0) return 4;
    if (libseat_close_device(seat, device) < 0) return 5;
    close(fd);
    return libseat_close_seat(seat) < 0 ? 6 : 0;
}

int main(void) {
    if (geteuid() != 0) return 70;
    int retained = dup(FIRST_FD);
    if (retained == -1) return 5;

    int enabled = 0;
    int result = run_backend(&enabled);
    if (result != 0 || enabled != 1) return 10 + result;

    char value[32];
    if (snprintf(value, sizeof value, "%d", retained) < 0) return 20;
    if (setenv("SEATD_FD", value, 1) == -1) return 21;
    enabled = 0;
    result = run_backend(&enabled);
    if (result != 0 || enabled != 1) return 30 + result;
    return 0;
}
EOF
cc "$work/client.c" -I"$libseat_repo/include" -L"$work/libseat-build" \
    -Wl,-rpath,"$work/libseat-build" -lseat -o "$work/client"
cargo build --manifest-path "$repo/Cargo.toml" \
    -p seatd-server --features test-support --example inherited-libseat-server
sudo "$repo/target/debug/examples/inherited-libseat-server" "$work/client"
echo "Rust seatd-server/patched libseat close-reopen compatibility test passed"
