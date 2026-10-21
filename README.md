# multiseat

Rust topology and orchestration tools for one seatd process per udev seat.

- `multiseatd` takes a fresh udev snapshot at startup and on explicit refresh,
  compares it with immutable deployed service manifests, and controls transient
  seatd services through s6 CLI tools.
- `multiseatctl` lists published seats and previews/confirms disruptive refresh.
- `seat-launch` remains a root development launcher for passing a connected
  seat socket to an unprivileged libseat compositor.

The workspace pins `seat-udev` through exact SSH tag `v0.1.0`. It does not
implement the seatd wire protocol and does not monitor udev continuously.
aclad retains its independent udev monitor.

## Runtime model

A fixed nested `s6-svscan` watches:

```text
/run/multiseatd/scandir/
```

multiseatd generates immutable services under:

```text
/run/multiseatd/services/<seat>-<configuration-hash>/
```

and attaches them with `s6-svlink`. Each service executes `seatd -n 3` with:

```text
SEATD_SEAT=<seat>
SEATD_SOCK=/run/multiseatd/<seat>.sock
SEATD_VTBOUND=0|1
SEATD_DEVICES=<comma-separated-static-allowlist>
```

The socket is mode 0600. SDDM's root helper connects before dropping
privileges and passes the actual descriptor number only to a supported
seat-owning compositor through `SEATD_FD`.

Deployed state comes from generated `config.json` manifests and s6 state, not
an in-memory cache or `/proc/<pid>/environ`. Unchanged ready seats are left
alone. A changed seat is removed, stopped, unlinked, regenerated, linked,
awaited, and published again.

## Explicit refresh

Topology does not change until requested:

```sh
sudo multiseatctl list
sudo multiseatctl refresh seat1
sudo multiseatctl refresh --force
```

The CLI prints old/new allowlists, any sessiond sessions it can query, and a
warning that graphical sessions may be terminated. Without `--force` it asks
for confirmation. The D-Bus mutation also requires an explicit confirmation
argument and independently verifies that the caller uid is root. Refreshes are
serialized.

D-Bus:

- service: `org.decoupling.multiseatd1`
- object: `/org/decoupling/multiseatd1`
- interface: `org.decoupling.multiseatd1.Manager`
- methods: `ListSeats`, `PreviewRefresh`, root-only `Refresh`
- signals: `SeatNew`, `SeatRemoved`

## Build and tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Unit tests use normalized configuration and fake topology/service backends.
The generated-service test does not need a running s6 installation. The root
end-to-end harness additionally requires the sibling patched seatd/libseat
checkout, D-Bus and s6 tools:

```sh
./tests/root-e2e.sh
```

Run root harnesses as the normal user; they invoke passwordless `sudo`
internally. Missing live prerequisites return status 77.

For headless test environments, hidden `--test-seat seat1` adds an empty,
non-VT test seat to each fresh snapshot. It does not grant any device.
