# multiseat

Linux seat topology, embedded seatd actors and direct-access UID leases.

- `multiseatd` continuously consumes monitor-first udev topology. It runs one
  synchronous coordinator thread and one seatd actor thread per discovered seat.
- `seatd-server` implements the corrected seatd server protocol, VT arbitration,
  dynamic device authorization and injected socketpair clients. The existing
  `seatd` fork remains the C libseat source and behavioral reference.
- `seat-udev` owns topology, classification, static direct-access discovery and
  persistent assignments. Its source revision and license are preserved in
  `crates/seat-udev/SOURCE.md`; the standalone repository is unchanged. Seat
  and uaccess rules are checked into `dist/udev` for the later package cutover
  (provenance in `dist/udev/SOURCE.md`) and are not installed by this source.
- `lib-multiseatd` (`libmultiseatd` in Rust) is the blocking D-Bus client used by
  loginctl and seat-launch.
- `seat-launch` is a root development launcher: it obtains a client FD through
  D-Bus, drops to the selected user and executes the compositor.

## Source-only transition

This source replaces the old per-seat s6 process orchestration. It does not
change installed services or package recipes, retire the standalone seatd,
seat-udev or aclad repositories, or deploy to a VM. **Do not run integrated ACL
management alongside aclad against live devices.** Package ownership, service
changes and atomic runtime cutover require a separate reviewed deployment plan.

## Execution and access policy

The main thread polls D-Bus, the udev monitor, actor events and shutdown signals.
There is no Tokio, async executor, D-Bus worker or filesystem seat listener.
Actors prioritize commands over bounded client-message batches. Seat0 alone is
VT-bound; its actor owns VT ioctls, signalfd and the delayed-release timer.
Operations exceeding 100 ms produce warnings, not hard latency guarantees.

Before enumerating devices, changing ACLs or creating actors, the daemon takes
an exclusive lock on `/run/multiseatd.lock`. It holds that lock through final
cleanup and never unlinks the lock inode. `--lock-file PATH` selects an explicit
alternative in a root-owned directory; instances managing the same resources
must use the same lock. D-Bus publication still waits for ready initial state.

```text
brokered: DRM / evdev / hidraw -> seatd-server only
uaccess minus brokered        -> direct-access UID ACLs only
```

Device movement and reclassification remove old brokered authorization and
old direct-access ACLs before adding either resource on the destination.
Removed protocol device IDs remain closeable; remove/re-add creates a new
kernel file description. Hidraw fails closed without revocation support.
Failed revocation ioctls retain the server device FD independently of client
membership. Actors retry cleanup on a one-second deadline without an extra
thread. The transport and UID lease still end immediately; explicit revoke
reports failure until cleanup completes. No successor activation or destination
authorization may bypass pending revocation. Even after the two-second client
disable deadline, a failed device ioctl prevents kernel VT release until retry
succeeds. Process shutdown remains best effort if the kernel keeps rejecting
revocation.
See `crates/seatd-server/BEHAVIOR.md` for test mapping and kernel limitations.

The ACL backend is adapted from aclad revision
`b4347297c895c47074555ea8eb0a9432e8a3cb0e` (GPL-3.0-only), without modifying that
repository. It preserves owner, group, named-group and other entries, replaces
named users with the desired set and recalculates the mask. Startup clears
stale named-user entries on known direct nodes before publishing seats.
Multiple client leases for the same `(seat, uid)` are reference-counted.
Failed convergence is retained for retry; failed revocation never resurrects
a client lease. ACL operations pin an inode with `O_PATH` and use its procfs
FD path, avoiding device-I/O opens and pathname replacement races during the
write. Old-node cleanup checks the recorded identity and skips unrelated
replacement inodes. Topology batches and periodic retries revalidate actual
ACL state, even when the pathname and desired UIDs are unchanged. Current
udev tags, not historical sticky tags, determine access and seat lifetime;
older libudev implementations retain an explicit compatibility path.

Client IDs are canonical UUIDv7 strings, opaque and valid for one daemon
lifetime, not authentication tokens. Final endpoint EOF removes the lease.
Protocol close without EOF resets a connection to `connected` and preserves
its lease. Explicit revocation is idempotent. Unexpected actor failure or bus
loss triggers best-effort process-wide cleanup and nonzero exit. The daemon
never restarts itself.

## Root-only D-Bus contract

```text
service:   org.decoupling.multiseatd1
path:      /org/decoupling/multiseatd1
interface: org.decoupling.multiseatd1.Manager

ListSeats()                       -> as
CreateClient(seat: s, uid: u)      -> (client_id: s, connection: h)
RevokeClient(client_id: s)
ListClients()                     -> a(ssus)
SeatNew(seat: s)                   [signal]
SeatRemoved(seat: s)               [signal]
```

Client rows are `(id, seat, uid, state)`. States are `connected`, `waiting`,
`active`, `pending-disable` and `disabled`. Every method independently checks
peer UID; bus policy also restricts calls and signal reception to root.
`PreviewRefresh` and `Refresh` are removed. Seats are published only after
initial device state is ready and reject clients before removal teardown.

```sh
# Development use only, on a correctly cut-over system:
seat-launch --seat seat1 --user alice -- compositor ...
```

Only the selected child inherits `SEATD_FD`, `LIBSEAT_BACKEND=seatd` and
`XDG_SEAT`; the launcher closes its endpoint before waiting and revokes the ID
on exit or exec failure. Do not use duplicated endpoints concurrently.

## Build and validation

Dependencies include Linux udev, libdbus and libacl development libraries.
Unit tests rewrite temporary ACL files only; they never modify device ACLs.

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
./tests/root-seatd-server-libseat.sh
./tests/root-dbus.sh
./tests/root-e2e.sh
```

Run root harnesses as the normal user; they use passwordless sudo. Missing
prerequisites return status 77. They use temporary private buses, socketpairs,
non-VT synthetic seats and a patched seatd/libseat checkout, not s6, physical
seat devices or the live system bus.

The harnesses do not assume a sibling checkout. `tests/libseat-source.sh`
clones the pinned fork from
`https://github.com/shaitan-machine1/seatd.git` at commit `7813c08` into the
gitignored `.test-libseat/` cache on first use, and skips with status 77 when
the clone is unavailable. `LIBSEAT_URL`, `LIBSEAT_COMMIT` and `LIBSEAT_CACHE`
override the remote, revision and cache location; `LIBSEAT_REPO` can point at
an existing local checkout instead.

The first harness covers real libseat open/device/close and sequential reopen
through a retained duplicate. The private-bus harness verifies independent
root authentication, restricted signals, FD and duplicate lifetime, explicit
revocation, singleton exclusion without changing a live temporary ACL grant,
name ownership, bus-loss cleanup and the two-thread count for one synthetic
seat. The end-to-end harness uses the real daemon and seat-launch,
runs patched libseat as nobody and tests exec-failure cleanup.

Hidden `--test-only --test-seat seat-test --lock-file /TEMP/DIR/daemon.lock`
ignores live topology and creates only synthetic non-VT seats; an explicit
disposable lock path is mandatory. Hidden `--test-device` requires the
`multiseatd/test-support` build feature and accepts only `/dev/null` as a
synthetic evdev stand-in. Its explicit test backend validates the open but
performs no device revocation ioctl. `--test-direct-node` selects a disposable
ACL fixture. Neither test hook is production policy; the harnesses never treat
`/dev/null`'s `ENOTTY` as evidence that a real device was revoked.
The unit suites also exercise failed-ioctl retention and retry, truncated
ancillary FD cleanup, real libudev tag parsing, same-path inode replacement and
externally reset ACLs. Real VT, DRM, evdev and hidraw hardware validation is deferred to the
separate deployment phase.
