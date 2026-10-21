# multiseat

M1's central seat broker and compositor launcher:

- `multiseatd`: one root process, one monitor-first `seat_udev::Context`, one
  isolated seat unit/socket per current udev seat, a seatd-compatible wire
  server, VT handling for seat0, queued activation for non-VT seats, device
  authorization/revocation, D-Bus seat discovery, and one optional opener
  pidfd per client;
- `seat-launch`: root development launcher which connects first, inherits the
  connection as fd 3, initializes groups, drops uid/gid and executes a normal
  libseat compositor.

The workspace pins `seat-udev` through exact SSH tag `v0.1.0`. The paired
libseat fork preserves API/ABI and adds `SEATD_FD` plus a Linux pidfd attached
to the unchanged `CLIENT_OPEN_SEAT` packet.

## Build and tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The tests use fake authority/device backends and real Unix socket ancillary
fd passing. The protocol e2e proves that opener death removes a client while a
socket copy is still live. No DRM, input device, monitor, seat or VT is needed.

For local daemon testing on a headless host, the hidden `--test-seat seat1`
option creates a non-VT socket independently of udev. It never grants device
access outside the real central `seat_udev` authority. The committed root
end-to-end harness uses that path with a private D-Bus daemon, `seat-launch`,
and a temporary build of the patched libseat fork:

```sh
./tests/root-e2e.sh
```

It requires passwordless `sudo`, Meson/Ninja, and the sibling libseat checkout
(override with `LIBSEAT_REPO=/path/to/libseat`); missing prerequisites return
status 77.

## Runtime

- sockets: `/run/multiseatd/<seat>.sock`;
- D-Bus: `org.decoupling.multiseatd1`, object
  `/org/decoupling/multiseatd1`, interface
  `org.decoupling.multiseatd1.Manager`;
- methods/signals: `ListSeats`, `SeatNew`, `SeatRemoved`.

Install `dist/dbus/org.decoupling.multiseatd1.conf`. The Artix s6 longrun must
depend on the udev coldplug/settle oneshot and system D-Bus. There are no
per-seat s6 services.

```sh
sudo seat-launch --seat seat0 --user alice -- sway
```

Live DRM/input and VT verification is deferred to the two-seat M5 VM.

## Design notes

- **The kernel owns the current VT, not the daemon.** A VT-bound seat
  re-reads the foreground terminal (`VT_GETSTATE` on `/dev/tty0`) whenever a
  client opens the seat. The daemon is only told about switches it arbitrates
  itself, so a value cached at startup goes stale the moment a getty, `chvt`
  or a display manager jumping to the session VT moves the foreground; the
  seat would then be bound to a terminal nobody is on, and the real terminal
  would keep whatever VT owner registered itself last.
- **Logs at info by default.** An unset `RUST_LOG` means "error only" in
  `tracing`, which leaves a supervised daemon with an empty log. Seat opens,
  activations, VT takeovers/releases and every failed VT ioctl are logged.
