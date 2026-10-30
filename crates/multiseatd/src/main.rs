use clap::{Parser, ValueEnum};
use multiseatd::acl::LibAcl;
use multiseatd::dbus::Bus;
use multiseatd::instance::InstanceLock;
use multiseatd::orchestrator::{OperationTimer, Orchestrator};
use multiseatd::topology::Snapshot;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use seat_udev::{Context, SeatName};
use seatd_server::{AllowedDevice, DeviceId, DeviceKind, ServerRuntime};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BusKind {
    System,
    Session,
}
#[derive(Debug, Parser)]
#[command(
    version,
    about = "Continuously coordinate embedded seatd actors and direct-access UID leases"
)]
struct Args {
    #[arg(long, value_enum, default_value_t = BusKind::System)]
    bus: BusKind,
    /// Instance lock in a root-owned directory (default /run/multiseatd.lock).
    #[arg(long)]
    lock_file: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_seat: Vec<SeatName>,
    /// Use synthetic non-VT seats only; never enumerate or change live devices.
    #[arg(long, hide = true)]
    test_only: bool,
    /// Explicit synthetic authorization for integration tests, never production.
    #[arg(long, hide = true, requires = "test_only")]
    test_device: Vec<PathBuf>,
    /// Disposable direct-access ACL fixtures only; never used with live udev.
    #[arg(long, hide = true, requires = "test_only")]
    test_direct_node: Vec<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "multiseatd=info,seatd_server=info".into()),
        )
        .init();
    let args = Args::parse();
    if unsafe { libc::geteuid() } != 0 {
        return Err("multiseatd must run as root".into());
    }
    if !args.test_device.is_empty()
        && (!cfg!(feature = "test-support")
            || args
                .test_device
                .iter()
                .any(|path| path != std::path::Path::new("/dev/null")))
    {
        return Err(
            "--test-device requires the test-support build and only accepts /dev/null".into(),
        );
    }
    if args.test_only && args.lock_file.is_none() {
        return Err("--test-only requires an explicit disposable --lock-file".into());
    }
    let lock_path = args
        .lock_file
        .as_deref()
        .unwrap_or(std::path::Path::new("/run/multiseatd.lock"));
    // Declared before every resource owner, so even error unwinding finishes
    // actor/ACL cleanup before another instance can acquire the lock.
    let _instance = InstanceLock::acquire(lock_path)?;
    let mut shutdown_mask = SigSet::empty();
    shutdown_mask.add(Signal::SIGTERM);
    shutdown_mask.add(Signal::SIGINT);
    shutdown_mask.thread_block()?;
    let mut shutdown_signal = SignalFd::with_flags(
        &shutdown_mask,
        SfdFlags::SFD_CLOEXEC | SfdFlags::SFD_NONBLOCK,
    )?;
    // Must precede all actor threads; their signal masks are inherited.
    let runtime = ServerRuntime::initialize()?;
    let mut context = if args.test_only {
        None
    } else {
        Some(Context::new()?)
    };
    let initial = if let Some(context) = &context {
        Snapshot::from_context(context, &args.test_seat)?
    } else {
        if args.test_seat.is_empty() || args.test_seat.contains(&SeatName::seat0()) {
            return Err("--test-only requires at least one non-VT --test-seat".into());
        }
        let mut snapshot = Snapshot {
            seats: args.test_seat.iter().cloned().collect(),
            ..Snapshot::default()
        };
        snapshot.direct.insert(
            args.test_seat[0].clone(),
            args.test_direct_node.iter().cloned().collect(),
        );
        for path in &args.test_device {
            let device = AllowedDevice::canonicalized(
                DeviceId::new(path.to_string_lossy()),
                path,
                std::fs::metadata(path)?.rdev(),
                DeviceKind::Evdev,
            )?;
            snapshot
                .brokered
                .entry(args.test_seat[0].clone())
                .or_default()
                .push(device);
        }
        snapshot
    };
    let mut coordinator = Orchestrator::new(runtime, LibAcl, &initial)?;
    #[cfg(feature = "test-support")]
    if args.test_only && !args.test_device.is_empty() {
        for seat in &initial.seats {
            coordinator.seed_test_actor(seat)?;
        }
    }
    coordinator.reconcile_topology(&initial, |_, _| {})?;
    // No D-Bus name is owned until initial ACL and brokered state are ready.
    let bus = Bus::new(matches!(args.bus, BusKind::Session))?;
    let result = run_loop(
        &args,
        &bus,
        &mut coordinator,
        &mut context,
        &mut shutdown_signal,
    );
    let cleanup = coordinator.shutdown();
    result?;
    cleanup?;
    Ok(())
}

fn run_loop(
    args: &Args,
    bus: &Bus,
    coordinator: &mut Orchestrator,
    context: &mut Option<Context>,
    shutdown: &mut SignalFd,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut immediate = false;
    let mut pending = None;
    let mut next_retry = Instant::now() + Duration::from_secs(1);
    let mut next_topology_retry = Instant::now();
    loop {
        if !bus.connected() {
            return Err("lost the D-Bus connection".into());
        }
        let watch = bus.watch();
        let deadline = if pending.is_some() {
            next_retry.min(next_topology_retry)
        } else {
            next_retry
        };
        let timeout = if immediate {
            0
        } else {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(1000) as u16
        };
        {
            // SAFETY: the bus connection owns this FD throughout the poll.
            let bus_fd = unsafe { BorrowedFd::borrow_raw(watch.fd) };
            let mut flags = PollFlags::POLLIN;
            if watch.write || bus.has_output() {
                flags |= PollFlags::POLLOUT;
            }
            let mut polls = vec![
                PollFd::new(bus_fd, flags),
                PollFd::new(coordinator.event_fd(), PollFlags::POLLIN),
                PollFd::new(shutdown.as_fd(), PollFlags::POLLIN),
            ];
            if let Some(context) = context.as_ref() {
                polls.push(PollFd::new(context.monitor_fd(), PollFlags::POLLIN));
            }
            match poll(&mut polls, timeout) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if shutdown.read_signal()?.is_some() {
            return Ok(());
        }
        // Actor failures and EOF take priority over new D-Bus work.
        coordinator.process_events()?;
        if let Some(context) = context.as_mut() {
            let _timer = OperationTimer::new("udev event batch");
            let mut changed = false;
            for _ in 0..64 {
                let Some(_events) = context.receive()? else {
                    break;
                };
                // Even a semantic duplicate may follow devnode recreation or
                // a udev ACL/mode reset. Recheck direct ACLs for every batch.
                changed = true;
            }
            if changed {
                pending = Some(Snapshot::from_context(context, &args.test_seat)?);
            }
        }
        if pending.is_some()
            && Instant::now() >= next_topology_retry
            && let Some(desired) = pending.as_ref()
        {
            let mut signal_error = None;
            match coordinator.reconcile_topology(desired, |new, seat| {
                if let Err(error) = bus.signal(new, seat) {
                    signal_error = Some(error);
                }
            }) {
                Ok(()) => {
                    pending = None;
                    next_topology_retry = Instant::now();
                }
                Err(error) => {
                    // A persistent ACL/ioctl failure must not turn into a poll
                    // spin or block unrelated D-Bus dispatch.
                    next_topology_retry = Instant::now() + Duration::from_secs(1);
                    tracing::error!(%error, "topology convergence failed; retaining desired state for retry");
                }
            }
            if let Some(error) = signal_error {
                return Err(error.into());
            }
        }
        if Instant::now() >= next_retry {
            if let Err(error) = coordinator.retry_access() {
                tracing::error!(%error, "brokered cleanup or ACL convergence retry failed");
            }
            next_retry = Instant::now() + Duration::from_secs(1);
        }
        immediate = bus.dispatch(coordinator)?;
    }
}
