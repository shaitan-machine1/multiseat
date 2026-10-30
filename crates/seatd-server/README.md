# seatd-server: embedded seat arbitration

This crate is a library, not the standalone `seatd` daemon. It implements the
seatd wire protocol and one-seat device arbitration so that `multiseatd` can
run one server actor per discovered seat in its own process.

The reference for "upstream" here is the seatd project's `seatd/seatd` server
(compared against commit `427b5d9`). Upstream remains the source of truth for
the protocol and for the C `libseat` client. This crate does not reimplement
libseat and does not accept its own socket connections.

## Behaviour shared with upstream

- The wire protocol: opcode numbering, framing, string and empty-message
  validation, `SEAT_OPENED` / `ENABLE_SEAT` / `DISABLE_SEAT` / `SEAT_DISABLED`
  / `SESSION_SWITCHED` ordering, `PING`/`PONG`, and error replies.
- One active client per seat. Only the active client may open devices.
- Duplicate opens of the same device by one client return the same record and
  increase its reference count; the per-client limit is 128 devices.
- Device classes DRM, evdev and hidraw, with `drmSetMaster`/`drmDropMaster`,
  `EVIOCREVOKE` and `HIDIOCREVOKE` used for activation and revocation.
- Canonicalisation of requested paths before use.
- VT mechanics for the VT-bound seat: foreground-VT refresh, `VT_RELDISP`
  handling, `VT_AUTO` on close, and session numbers tied to the current VT.
- Refused operations are reported to the client as protocol errors.

`BEHAVIOR.md` tracks the C-fork corrections that this port must preserve, and
maps them to Rust tests. It is the companion ledger to this document: this file
records where the embedded server intentionally differs from upstream, while
`BEHAVIOR.md` records the fork's own corrections and their coverage.

## Deliberate differences from upstream

### Transport and trust boundary

| Upstream `seatd` | This server |
| --- | --- |
| Binds and listens on a Unix socket (compile-time default `/run/seatd.sock`), optionally chowned with `-u`/`-g`. | Has no listener. Each client is an already-connected `AF_UNIX` `SOCK_STREAM` pair created by `multiseatd` and injected into the seat actor. |
| Authenticates local peers with `SO_PEERCRED` and records pid/uid/gid. Any permitted local process may connect and open the seat. | Never looks at peer credentials. The authorization boundary is `multiseatd`'s root-only D-Bus `CreateClient`, which also owns the client ID and the per-UID direct-access lease. |
| `seatd-launch` starts a private daemon around one command. | `seat-launch` asks `multiseatd` for one client; no daemon is started. |
| One daemon process, one seat: `client_get_seat_name()` returns the fixed string `seat0` (upstream `TODO: Look up seat for session`). | One actor per discovered seat. `SEAT_OPENED` reports the actor's own seat name, and the seat set comes from udev topology. |
| Daemon concerns in the same binary: `-n` readiness fd, `-u`/`-g` socket ownership, `-z` seatd-launch mode, `-l` log level, `-v`, daemonization, wscons/BSD. | None of these exist here. Process lifecycle, readiness, logging and shutdown belong to `multiseatd`; this crate is Linux-only and has no wscons or BSD device class. |

### Device authorization

Upstream has no per-device policy: an active client may open any path whose
prefix matches a supported class (`/dev/input/event*`, `/dev/dri/*`,
`/dev/hidraw*`, wscons entries). This server instead requires an explicit
authorization set:

- `add_device`, `replace_devices` and `remove_device` maintain the authorized
  devices, each with an `id`, canonical path, device number and generation.
- Paths are canonicalized, and the same canonical path cannot be authorized
  twice under different IDs.
- After opening, the kernel file description's device number must still match
  the authorized identity.
- Replacing an authorization with a different identity, or removing it,
  revokes all matching open records and denies further opens. Re-adding the
  same path starts a new generation and a new kernel file description.
- Revoked records are never reactivated by a later session switch.

The device ACLs, topology discovery and which devices are brokered are owned by
`multiseatd` and `seat-udev`, not by this crate.

### Arbitration

- **Non-VT seats activate a newly opened session.** Upstream queues the new
  client until the *currently active* client requests `CLIENT_SWITCH_SESSION`.
  That is unusable for a display manager: the successor session number is
  assigned only when the new client opens, and a greeting compositor never
  requests the switch. This server follows logind instead: on a seat without
  VTs, joining a seat retires the active client (its devices are deactivated,
  it is told `DISABLE_SEAT`, and its state becomes `disabled`) and activates the
  newcomer in the same step. Covered by
  `state::tests::non_vt_newcomer_takes_the_seat_from_an_active_client`.
  VT-bound seats still keep upstream arbitration: the kernel VT switch decides,
  and a second membership is refused while another client is active
  (`state::tests::vt_seat_still_rejects_a_membership_while_another_client_is_active`).
- **A non-VT seat never touches VT ioctls.** Only the VT-bound actor owns
  terminal state; `handle_vt_release`, `handle_vt_acquire` and
  `handle_vt_release_timeout` return without action on other seats.
- **VT release waits for the client.** The kernel release is acknowledged only
  after the departing client has been disabled (acknowledgement, disconnect or
  a two-second actor deadline), and `VT_AUTO` is restored when the VT is closed.
  Upstream acknowledges `VT_RELDISP` immediately after sending `DISABLE_SEAT`.
- **Protocol close does not end the connection.** `CLIENT_CLOSE_SEAT` releases
  membership, devices, arbitration and the assigned session, returning the
  connection to `connected`; it may open a seat again. Upstream leaves the
  client unsuitable for reuse.
- **Failed revocation is retained.** A failed `drmDropMaster`, `EVIOCREVOKE` or
  `HIDIOCREVOKE` keeps the record and its pending deactivation, blocks
  successor activation, handover, new authorization and seat cleanup, and is
  retried. `Action::CleanupFailed` reports the condition to the embedder.
  Upstream logs the failure and drops the device record.
- **Explicit revocation is idempotent.** `revoke_client` may be called for a
  client that already reached EOF, and failed revocations remain retryable
  independently of the live client table.

### Robustness and data model

- **Ancillary descriptors are owned.** Received descriptors are checked for
  `MSG_CTRUNC` and every received descriptor is closed before a malformed or
  overflowing message is rejected. One descriptor accompanies each successful
  `OPEN_DEVICE` reply; unsent actions close theirs.
- **Bounded queues.** Input and output are bounded, and partial writes are
  retained rather than blocking the actor.
- **No poll loop or global state.** The state machine is pure and synchronous.
  Each actor owns its sockets, an eventfd command mailbox and a timerfd for the
  VT deadline, so multiple seats run independently in one process.
- **Teardown revokes.** Dropping the state (including an actor panic) deactivates
  every still-active device and retries pending revocations. Upstream relies on
  process exit to close its descriptors.

## What still comes from the C fork

`libseat` itself, including inherited-`SEATD_FD` discovery, the seatd wire
client and its EOF handling, remains the C implementation. Protocol and
descriptor compatibility are exercised against the real client by
`tests/root-seatd-server-libseat.sh`.
