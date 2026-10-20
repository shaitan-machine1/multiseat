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
access outside the real central `seat_udev` authority.

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
