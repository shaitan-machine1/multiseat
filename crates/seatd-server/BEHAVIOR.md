# seatd server porting ledger

Reference: `seatd` fork commit `7813c08`. This ledger is a work in progress;
entries marked missing are not evidence of parity. Test names refer to this
crate unless a C test path is explicitly given.

| Fork correction | Rust coverage and status |
| --- | --- |
| `22312c7` string validation | `protocol::tests::paths_require_exact_single_nul_termination`, `protocol::tests::seat_names_are_validated_before_encoding` |
| `396e81d` trailing data | `protocol::tests::fixed_size_messages_reject_trailing_data` and `paths_require_exact_single_nul_termination` cover every client payload shape |
| `37aef8a` accept result | Not applicable: no listener adapter |
| `cf4b466` duplicate open | `state::tests::duplicate_device_open_reuses_the_record_and_reference_count` |
| `d023c7c` lowest free session | `state::tests::non_vt_sessions_reuse_the_lowest_free_number` |
| `4cef6e6` enable failure rollback | `actor::tests::disconnect_during_enable_delivery_rolls_back_vt_activation` verifies actor-side rollback and VT cleanup |
| `9d1f83e` departing VT cleanup | `state::tests::disconnect_and_timeout_complete_a_pending_vt_release` verifies cleanup uses the departing session |
| `4bcdc81` restore VT_AUTO | `LinuxVtOps::close` restores `VT_AUTO`; `state::tests::disconnect_and_timeout_complete_a_pending_vt_release` covers the fake-kernel close path |
| `da793e9` inherited SEATD_FD | Retained in the C libseat fork and covered against the Rust actor by `tests/root-seatd-server-libseat.sh` |
| `9bb3aab` canonical authorization | `state::tests::authorization_is_empty_by_default_and_matches_canonical_paths_only`, `device::tests::post_open_device_number_must_match`, and `non_device_paths_are_rejected` |
| `d9d0125` delayed VT release | `state::tests::vt_release_without_an_active_client_is_immediate`, `vt_release_waits_for_disable_ack_and_late_ack_is_idempotent`, `disconnect_and_timeout_complete_a_pending_vt_release`, and `tty7_to_tty8_handoff_releases_before_acquiring_and_enabling` |
| `7813c08` terminal EOF | `connection::tests::fragmented_packets_and_terminal_eof_are_preserved`, `actor::tests::eof_reports_client_gone`; C libseat EOF correction retained |

## C test suite mapping

- `tests/connection.c`: the Rust connection uses framed queues rather than the
  C byte restore API. Fragmentation, coalescing, bounded input/output,
  descriptors, descriptor cleanup on malformed input, buffered EOF and
  terminal EOF have direct coverage in `connection::tests` and
  `protocol::tests`.
- `tests/seat-allowlist.c`: empty authorization, canonical-path matching,
  post-open device-number validation and non-device rejection are covered by
  the state and device tests listed above. Dynamic remove/re-add coverage is in
  `state::tests::remove_and_readd_requires_a_new_kernel_file_description`.
  Dynamic authorization extensions and their failure semantics are detailed below.
- `tests/seat-vt.c`: release without client, acknowledgement, disconnect,
  timeout and tty7-to-tty8 handoff have direct deterministic state-machine
  coverage. Real VT ioctl validation remains a deployment-phase hardware test.
- `tests/inherited-fd.c`, `tests/seatd-eof.c`: C client behavior remains in
  the fork; `tests/root-seatd-server-libseat.sh` runs the patched real libseat
  against this server, including protocol close/reopen through a retained
  duplicate and final EOF. Invalid inherited-FD parsing remains covered by the
  C fork's own test.
- `tests/configured-server.c`: socket injection replaces the listener and
  configuration interfaces. `actor::tests::injected_connections_are_isolated_per_seat_actor`
  covers per-seat protocol isolation.

The Rust server deliberately permits protocol close/reopen on one socket:
`state::tests::close_then_open_reuses_one_live_protocol_connection` and
`actor::tests::actor_supports_close_then_reopen_on_one_socket` cover this
planned divergence. `tests/root-seatd-server-libseat.sh` covers sequential
real-libseat use through a deliberately retained duplicate.

## Dynamic authorization extensions

Device commands canonicalize their paths and reject ambiguous IDs or paths
before changing authorization. An unchanged replacement retains its generation
and existing open records. Any identity change or removal denies further opens
and deactivates all matching open records. A removed protocol device ID remains
closeable until its duplicate-open references reach zero; adding the same
path again uses a new authorization generation and a new kernel file description.
Revoked DRM records are never reactivated during a later session switch.

Removal is idempotent. If a revocation ioctl fails, the command reports failure,
retains the pending active record and retries on subsequent removal/replacement.
No new authorization is added while a failed revocation remains pending.
Replacement attempts every removal before reporting errors. A successful actor
command acknowledgement means all required device deactivations have completed.
An ioctl failure cannot itself guarantee that a client-held FD has lost access;
callers must not authorize the device elsewhere after such a failure.

Coverage:

- `state::tests::dynamic_removal_revokes_all_kinds_and_preserves_duplicate_close_bookkeeping`
- `state::tests::replacements_preserve_unchanged_opens_and_revoke_changed_identity`
- `state::tests::invalid_device_commands_leave_authorization_and_opens_unchanged`
- `state::tests::failed_removal_denies_opens_blocks_readd_and_retries_the_ioctl`
- `state::tests::replacement_attempts_all_revocations_before_reporting_failure`
- `state::tests::removed_drm_records_are_never_reactivated_after_a_session_switch`
- `actor::tests::device_removal_is_acknowledged_after_revocation_and_still_accepts_close`
- `state::tests::removal_completes_when_vt_acknowledgement_fails`
- `device::tests::opened_descriptors_have_defensive_flags`
- `device::tests::a_canonical_path_replaced_by_a_symlink_is_not_followed`

The coordinator-side `multiseatd::brokered` reconciler acknowledges every
source removal before any destination addition, including swaps between seats
and brokered/direct reclassification. Its tests cover removal/addition
failures, retries and topology returning to a source seat after a failed move.
`multiseatd::orchestrator` wires it into the continuously monitored coordinator
and orders brokered and direct-access teardown before either addition.

`SeatState`'s `Drop` deactivates every still-active device, so an actor panic
or event-loop failure does not merely close our descriptor while a client copy
remains usable. `actor::tests::actor_panic_deactivates_open_devices_and_reports_a_critical_failure`
covers that path and the resulting `ActorFailed` event; the coordinator treats
it as critical, rejects new work, tears down clients and exits.

## Failure-path safety regressions

Client EOF, explicit revoke, protocol seat close and final device close never
forget a device whose revocation ioctl failed. Its server FD moves into retired
cleanup storage, independent of membership and the external UID lease. Normal
disable failures remain retryable too. Device commands and a one-second actor
deadline retry cleanup without a worker thread or command polling sleeps.
`CleanupFailed` actions report errors without discarding client-gone events;
explicit revocation closes the transport even when it returns an error.
Pending cleanup blocks successor activation and new authorization. Device-command
completion also resumes arbitration after the last failed ioctl succeeds.

The two-second VT deadline forces the client disabled, but never overrides a
failed device revocation. Kernel release and departing-terminal restoration
wait for successful device cleanup; a disconnect cannot implicitly release the
VT early through `VT_AUTO` restoration. Final process teardown remains best
effort and cannot promise revocation if a driver persistently refuses it.

Coverage:

- `state::tests::teardown_keeps_failed_opens_without_resurrecting_membership`
- `state::tests::close_device_failure_is_retained_and_other_devices_are_attempted`
- `state::tests::failed_disable_blocks_successor_until_retry_succeeds`
- `state::tests::failed_device_revoke_blocks_kernel_release_even_after_ack_timeout_or_eof`
- `actor::tests::failed_revoke_closes_transport_reports_failure_and_retries_without_a_client`
- `multiseatd::orchestrator::tests::failed_cleanup_blocks_moves_but_not_lease_or_acl_removal`

Ancillary receive ownership is established before rejecting `MSG_CTRUNC`.
`connection::tests::truncated_ancillary_data_closes_every_installed_descriptor`
and `actor::tests::unexpected_and_truncated_rights_disconnect_without_leaking_writers`
use pipe-writer EOF to detect leaks, independently of parallel tests' FD counts.
The Linux kernel closes excess rights that do not fit; the receiver closes all
rights that were installed into its ancillary buffer.

Kernel limits remain those of the C server: DRM revocation drops master, not
all possible operations on the FD; evdev/hidraw revocation is permanent for that
open file description. The tests use fake device operations; real driver ioctl
validation is deferred to the deployment phase.
