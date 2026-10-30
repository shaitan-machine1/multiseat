use crate::access::AccessManager;
use crate::acl::{AclOps, LibAcl};
use crate::brokered::BrokeredDevices;
use crate::topology::Snapshot;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use seat_udev::SeatName;
use seatd_server::{ClientId, ClientState, SeatConfig, SeatEvent, SeatHandle, ServerRuntime};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

pub type ClientRow = (String, String, u32, String);

struct ClientLease {
    seat: SeatName,
    uid: u32,
    state: ClientState,
}

pub struct Orchestrator<A: AclOps = LibAcl> {
    runtime: ServerRuntime,
    actors: BTreeMap<SeatName, SeatHandle>,
    ready: BTreeSet<SeatName>,
    clients: BTreeMap<ClientId, ClientLease>,
    // No live lease/UID remains here: only failed brokered cleanup to retry.
    revoking: BTreeMap<ClientId, SeatName>,
    access: AccessManager<A>,
    brokered: BrokeredDevices,
    current: Snapshot,
}

impl<A: AclOps> Orchestrator<A> {
    pub fn new(runtime: ServerRuntime, acl: A, initial: &Snapshot) -> io::Result<Self> {
        // Clear stale ACLs before spawning or publishing any actor.
        let access = AccessManager::new(acl, initial.direct.clone())?;
        Ok(Self {
            runtime,
            actors: BTreeMap::new(),
            ready: BTreeSet::new(),
            clients: BTreeMap::new(),
            revoking: BTreeMap::new(),
            access,
            brokered: BrokeredDevices::default(),
            current: Snapshot::default(),
        })
    }

    /// Explicit synthetic fixture injection; unavailable in production builds.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn seed_test_actor(&mut self, seat: &SeatName) -> io::Result<()> {
        if seat == &SeatName::seat0() || !self.ready.is_empty() || self.actors.contains_key(seat) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid synthetic actor injection",
            ));
        }
        let actor = self.runtime.spawn_with_device_ops(
            SeatConfig {
                name: seat.to_string(),
                vt_bound: false,
            },
            Box::new(seatd_server::NullDeviceOps),
        )?;
        self.actors.insert(seat.clone(), actor);
        Ok(())
    }

    pub fn event_fd(&self) -> BorrowedFd<'_> {
        self.runtime.event_fd()
    }
    pub fn list_seats(&self) -> Vec<String> {
        self.ready.iter().map(ToString::to_string).collect()
    }
    pub fn list_clients(&self) -> Vec<ClientRow> {
        self.clients
            .iter()
            .map(|(id, lease)| {
                (
                    id.to_string(),
                    lease.seat.to_string(),
                    lease.uid,
                    state_name(lease.state).into(),
                )
            })
            .collect()
    }

    pub fn reconcile_topology(
        &mut self,
        desired: &Snapshot,
        mut signal: impl FnMut(bool, &SeatName),
    ) -> io::Result<()> {
        let _timer = OperationTimer::new("topology reconciliation");
        let removed = self
            .ready
            .difference(&desired.seats)
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for seat in removed {
            // Reject new clients before notification or resource teardown.
            self.ready.remove(&seat);
            signal(false, &seat);
            let clients = self
                .clients
                .iter()
                .filter(|(_, lease)| lease.seat == seat)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in clients {
                if let Err(error) = self.revoke_client(&id) {
                    first_error.get_or_insert(error);
                }
            }
        }
        // Create unpublished actors first, so destination devices can be seeded.
        for seat in &desired.seats {
            if !self.actors.contains_key(seat) {
                let actor = self.runtime.spawn_seat(SeatConfig {
                    name: seat.to_string(),
                    vt_bound: seat == &SeatName::seat0(),
                })?;
                self.actors.insert(seat.clone(), actor);
            }
        }
        // Remove BOTH resource classes globally before adding either. This is
        // necessary not just for moves, but brokered <-> direct reclassification.
        let retained_brokered = self
            .current
            .brokered
            .iter()
            .map(|(seat, devices)| {
                let kept = devices
                    .iter()
                    .filter(|device| {
                        desired
                            .brokered
                            .get(seat)
                            .is_some_and(|wanted| wanted.contains(device))
                    })
                    .cloned()
                    .collect();
                (seat.clone(), kept)
            })
            .filter(|(_, devices): &(_, Vec<_>)| !devices.is_empty())
            .collect();
        self.brokered.reconcile(&self.actors, &retained_brokered)?;
        let retained_direct = self
            .current
            .direct
            .iter()
            .map(|(seat, paths)| {
                let kept = paths
                    .iter()
                    .filter(|path| {
                        desired
                            .direct
                            .get(seat)
                            .is_some_and(|wanted| wanted.contains(*path))
                    })
                    .cloned()
                    .collect();
                (seat.clone(), kept)
            })
            .collect();
        self.access.set_nodes(retained_direct)?;
        self.access.set_nodes(desired.direct.clone())?;
        self.brokered.reconcile(&self.actors, &desired.brokered)?;
        if let Some(error) = first_error {
            return Err(error);
        }
        let stopped = self
            .actors
            .keys()
            .filter(|seat| !desired.seats.contains(*seat))
            .cloned()
            .collect::<Vec<_>>();
        for seat in stopped {
            self.actors
                .remove(&seat)
                .expect("actor exists")
                .shutdown()?;
        }
        for seat in desired
            .seats
            .difference(&self.ready)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.ready.insert(seat.clone());
            signal(true, &seat);
        }
        self.current = desired.clone();
        Ok(())
    }

    pub fn create_client(&mut self, seat: &SeatName, uid: u32) -> io::Result<(String, OwnedFd)> {
        let _timer = OperationTimer::new("client creation");
        if !self.ready.contains(seat) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "seat is not ready"));
        }
        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )?;
        let id = ClientId::new(uuid::Uuid::now_v7().to_string());
        self.actors[seat].add_client(id.clone(), server)?;
        if let Err(error) = self.access.acquire(id.clone(), seat.clone(), uid) {
            if let Err(rollback) = self.actors[seat].revoke_client(id.clone()) {
                tracing::error!(%id, %rollback, "failed to revoke rejected seatd client");
            }
            return Err(error);
        }
        self.clients.insert(
            id.clone(),
            ClientLease {
                seat: seat.clone(),
                uid,
                state: ClientState::Connected,
            },
        );
        Ok((id.to_string(), client))
    }

    pub fn revoke_client(&mut self, id: &ClientId) -> io::Result<()> {
        let _timer = OperationTimer::new("client revocation");
        if let Some(lease) = self.clients.remove(id) {
            self.revoking.insert(id.clone(), lease.seat);
        }
        // Always attempt ACL and brokered cleanup independently, even on error.
        let acl_result = self.access.revoke(id);
        let actor_result = self
            .revoking
            .get(id)
            .and_then(|seat| self.actors.get(seat))
            .map(|actor| actor.revoke_client(id.clone()))
            .unwrap_or(Ok(()));
        if actor_result.is_ok() {
            self.revoking.remove(id);
        }
        match (acl_result, actor_result) {
            (Err(error), Err(actor)) => {
                tracing::error!(%actor, "brokered revocation also failed");
                Err(error)
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }

    pub fn process_events(&mut self) -> io::Result<()> {
        let _timer = OperationTimer::new("actor events");
        self.handle_events(self.runtime.drain_events())
    }

    fn handle_events(&mut self, events: impl IntoIterator<Item = SeatEvent>) -> io::Result<()> {
        let mut first_error = None;
        for event in events {
            match event {
                SeatEvent::ClientGone {
                    client,
                    seat,
                    cleanup_pending,
                } => {
                    self.clients.remove(&client);
                    if cleanup_pending {
                        self.revoking.insert(
                            client.clone(),
                            SeatName::parse(seat).expect("actor seat name was validated"),
                        );
                    }
                    if let Err(error) = self.access.revoke(&client) {
                        tracing::error!(%client, %error, "EOF ACL cleanup needs retry");
                    }
                }
                SeatEvent::ClientStateChanged { client, state, .. } => {
                    if let Some(lease) = self.clients.get_mut(&client) {
                        lease.state = state;
                    }
                }
                SeatEvent::ActorFailed { seat, message } => {
                    self.ready.clear();
                    first_error.get_or_insert_with(|| {
                        io::Error::other(format!(
                            "critical seat actor failure for {seat}: {message}"
                        ))
                    });
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn retry_access(&mut self) -> io::Result<()> {
        let _timer = OperationTimer::new("resource cleanup retry");
        let mut first_error = None;
        for seat in self.revoking.values().cloned().collect::<BTreeSet<_>>() {
            let result = self
                .actors
                .get(&seat)
                .map(SeatHandle::retry_cleanup)
                .unwrap_or(Ok(()));
            match result {
                Ok(()) => self.revoking.retain(|_, owner| owner != &seat),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        // A brokered failure never suppresses unrelated ACL removals.
        if let Err(error) = self.access.reconcile() {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        let _timer = OperationTimer::new("daemon shutdown");
        self.ready.clear();
        let mut first_error = None;
        for id in self.clients.keys().cloned().collect::<Vec<_>>() {
            if let Err(error) = self.revoke_client(&id) {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = self.retry_access() {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.access.shutdown() {
            first_error.get_or_insert(error);
        }
        for (_, actor) in std::mem::take(&mut self.actors) {
            if let Err(error) = actor.shutdown() {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl<A: AclOps> Drop for Orchestrator<A> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub fn state_name(state: ClientState) -> &'static str {
    match state {
        ClientState::Connected => "connected",
        ClientState::Waiting => "waiting",
        ClientState::Active => "active",
        ClientState::PendingDisable => "pending-disable",
        ClientState::Disabled => "disabled",
    }
}

pub struct OperationTimer {
    operation: &'static str,
    start: Instant,
}
impl OperationTimer {
    pub fn new(operation: &'static str) -> Self {
        Self {
            operation,
            start: Instant::now(),
        }
    }
}
impl Drop for OperationTimer {
    fn drop(&mut self) {
        report_duration(self.operation, self.start.elapsed());
    }
}
fn report_duration(operation: &str, elapsed: Duration) -> bool {
    let slow = elapsed > Duration::from_millis(100);
    if slow {
        tracing::warn!(
            operation,
            elapsed_ms = elapsed.as_millis() as u64,
            "slow coordinator operation"
        );
    }
    slow
}

#[cfg(test)]
mod tests {
    use super::*;
    use seatd_server::protocol;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct FakeAcl(
        Rc<RefCell<BTreeMap<PathBuf, BTreeSet<u32>>>>,
        Rc<RefCell<bool>>,
    );
    use std::path::PathBuf;
    impl AclOps for FakeAcl {
        fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()> {
            if *self.1.borrow() {
                return Err(io::Error::other("ACL failure"));
            }
            self.0.borrow_mut().insert(path.into(), users.clone());
            Ok(())
        }
    }
    fn seat() -> SeatName {
        SeatName::parse("seat-test").unwrap()
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            seats: BTreeSet::from([seat()]),
            direct: BTreeMap::from([(seat(), BTreeSet::from(["/direct".into()]))]),
            ..Snapshot::default()
        }
    }
    fn setup() -> (Orchestrator<FakeAcl>, FakeAcl) {
        let acl = FakeAcl::default();
        let mut coordinator = Orchestrator::new(
            ServerRuntime::initialize().unwrap(),
            acl.clone(),
            &snapshot(),
        )
        .unwrap();
        coordinator
            .reconcile_topology(&snapshot(), |_, _| {})
            .unwrap();
        (coordinator, acl)
    }
    fn packet(stream: &mut std::os::unix::net::UnixStream) -> u16 {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut body = vec![0; u16::from_ne_bytes([header[2], header[3]]) as usize];
        stream.read_exact(&mut body).unwrap();
        u16::from_ne_bytes([header[0], header[1]])
    }
    fn drain_until(
        coordinator: &mut Orchestrator<FakeAcl>,
        condition: impl Fn(&Orchestrator<FakeAcl>) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition(coordinator) {
            coordinator.process_events().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    #[test]
    fn socket_eof_releases_leases_but_protocol_close_does_not() {
        let (mut coordinator, acl) = setup();
        let (id, fd) = coordinator.create_client(&seat(), 1000).unwrap();
        assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 7);
        let mut client = std::os::unix::net::UnixStream::from(fd);
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        assert_eq!(packet(&mut client), protocol::SERVER_SEAT_OPENED);
        assert_eq!(packet(&mut client), protocol::SERVER_ENABLE_SEAT);
        drain_until(&mut coordinator, |c| c.list_clients()[0].3 == "active");
        client
            .write_all(&protocol::empty(protocol::CLIENT_CLOSE_SEAT))
            .unwrap();
        assert_eq!(packet(&mut client), protocol::SERVER_SEAT_CLOSED);
        drain_until(&mut coordinator, |c| c.list_clients()[0].3 == "connected");
        assert_eq!(acl.0.borrow()[Path::new("/direct")], BTreeSet::from([1000]));
        drop(client);
        drain_until(&mut coordinator, |c| c.list_clients().is_empty());
        assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
    }
    #[test]
    fn transactional_failure_and_idempotent_forced_revocation() {
        let (mut coordinator, acl) = setup();
        *acl.1.borrow_mut() = true;
        assert!(coordinator.create_client(&seat(), 1000).is_err());
        assert!(coordinator.clients.is_empty());
        *acl.1.borrow_mut() = false;
        coordinator.retry_access().unwrap();
        let (id, fd) = coordinator.create_client(&seat(), 1000).unwrap();
        let id = ClientId::new(id);
        *acl.1.borrow_mut() = true;
        assert!(coordinator.revoke_client(&id).is_err());
        let mut stream = std::os::unix::net::UnixStream::from(fd);
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        assert!(coordinator.clients.is_empty());
        *acl.1.borrow_mut() = false;
        coordinator.revoke_client(&id).unwrap();
        assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
    }
    #[test]
    fn seat_removal_rejects_new_clients_and_forces_all_endpoints_closed() {
        let (mut coordinator, acl) = setup();
        let (_, one) = coordinator.create_client(&seat(), 1000).unwrap();
        let (_, two) = coordinator.create_client(&seat(), 1000).unwrap();
        let mut signals = Vec::new();
        coordinator
            .reconcile_topology(&Snapshot::default(), |new, seat| {
                signals.push((new, seat.clone()))
            })
            .unwrap();
        assert_eq!(signals, [(false, seat())]);
        assert!(coordinator.create_client(&seat(), 1001).is_err());
        for fd in [one, two] {
            let mut stream = std::os::unix::net::UnixStream::from(fd);
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        }
        assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
        assert!(coordinator.actors.is_empty());
    }
    #[test]
    fn critical_actor_failure_rejects_work_and_cleanup_closes_other_actors() {
        let (mut coordinator, acl) = setup();
        let (_, fd) = coordinator.create_client(&seat(), 1000).unwrap();
        assert!(
            coordinator
                .handle_events([SeatEvent::ActorFailed {
                    seat: seat().to_string(),
                    message: "injected panic".into()
                }])
                .is_err()
        );
        assert!(coordinator.list_seats().is_empty());
        assert!(coordinator.create_client(&seat(), 1001).is_err());
        coordinator.shutdown().unwrap();
        assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
        assert_eq!(
            std::os::unix::net::UnixStream::from(fd)
                .read(&mut [0])
                .unwrap(),
            0
        );
    }

    #[test]
    fn failed_cleanup_blocks_moves_but_not_lease_or_acl_removal() {
        use seatd_server::{AllowedDevice, DeviceId, DeviceKind, DeviceOps};
        use std::os::unix::fs::MetadataExt;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        struct ControlledOps(Arc<AtomicBool>, Arc<AtomicUsize>);
        impl DeviceOps for ControlledOps {
            fn open(&mut self, _: &AllowedDevice) -> io::Result<OwnedFd> {
                Ok(std::fs::File::open("/dev/null")?.into())
            }
            fn deactivate(&mut self, _: DeviceKind, _: &OwnedFd) -> io::Result<()> {
                self.1.fetch_add(1, Ordering::SeqCst);
                if self.0.load(Ordering::SeqCst) {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    Ok(())
                }
            }
            fn activate(&mut self, _: DeviceKind, _: &OwnedFd) -> io::Result<()> {
                Ok(())
            }
        }
        fn open_device(stream: &mut std::os::unix::net::UnixStream) {
            let path = b"/dev/null\0";
            let mut payload = (path.len() as u16).to_ne_bytes().to_vec();
            payload.extend_from_slice(path);
            stream
                .write_all(&protocol::packet(protocol::CLIENT_OPEN_DEVICE, &payload).unwrap())
                .unwrap();
        }
        for operation in ["revoke", "eof", "protocol-close", "seat-removal"] {
            let acl = FakeAcl::default();
            let source = seat();
            let target = SeatName::parse("seat-target").unwrap();
            let fail = Arc::new(AtomicBool::new(false));
            let attempts = Arc::new(AtomicUsize::new(0));
            let device = AllowedDevice::canonicalized(
                DeviceId::new("null"),
                "/dev/null",
                std::fs::metadata("/dev/null").unwrap().rdev(),
                DeviceKind::Evdev,
            )
            .unwrap();
            let initial = Snapshot {
                seats: BTreeSet::from([source.clone(), target.clone()]),
                direct: BTreeMap::from([
                    (source.clone(), BTreeSet::from(["/direct".into()])),
                    (target.clone(), BTreeSet::from(["/target".into()])),
                ]),
                brokered: BTreeMap::from([(source.clone(), vec![device.clone()])]),
            };
            let mut coordinator =
                Orchestrator::new(ServerRuntime::initialize().unwrap(), acl.clone(), &initial)
                    .unwrap();
            for (name, control) in [
                (&source, fail.clone()),
                (&target, Arc::new(AtomicBool::new(false))),
            ] {
                let actor = coordinator
                    .runtime
                    .spawn_with_device_ops(
                        SeatConfig {
                            name: name.to_string(),
                            vt_bound: false,
                        },
                        Box::new(ControlledOps(control, attempts.clone())),
                    )
                    .unwrap();
                coordinator.actors.insert(name.clone(), actor);
            }
            coordinator.reconcile_topology(&initial, |_, _| {}).unwrap();
            let (id, fd) = coordinator.create_client(&source, 1000).unwrap();
            let id = ClientId::new(id);
            let mut stream = std::os::unix::net::UnixStream::from(fd);
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
                .unwrap();
            packet(&mut stream);
            packet(&mut stream);
            open_device(&mut stream);
            let mut received =
                seatd_server::Connection::new(stream.try_clone().unwrap().into()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while received.received_fd_count() == 0 {
                received.read().unwrap();
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            let held = received.take_received_fd().unwrap();
            let (_, target_fd) = coordinator.create_client(&target, 1001).unwrap();
            let mut target_stream = std::os::unix::net::UnixStream::from(target_fd);
            target_stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            target_stream
                .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
                .unwrap();
            packet(&mut target_stream);
            packet(&mut target_stream);
            fail.store(true, Ordering::SeqCst);
            let moved = Snapshot {
                seats: BTreeSet::from([target.clone()]),
                direct: BTreeMap::from([(
                    target.clone(),
                    BTreeSet::from(["/direct".into(), "/target".into()]),
                )]),
                brokered: BTreeMap::from([(target.clone(), vec![device])]),
            };
            if operation == "revoke" {
                // ACL failure must not prevent the brokered ioctl attempt.
                *acl.1.borrow_mut() = true;
                assert!(coordinator.revoke_client(&id).is_err());
                assert!(attempts.load(Ordering::SeqCst) > 0);
                *acl.1.borrow_mut() = false;
                assert!(coordinator.retry_access().is_err());
            } else if operation == "eof" {
                drop(received);
                drop(stream);
                drain_until(&mut coordinator, |c| !c.clients.contains_key(&id));
            } else if operation == "protocol-close" {
                stream
                    .write_all(&protocol::empty(protocol::CLIENT_CLOSE_SEAT))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    coordinator.process_events().unwrap();
                    if coordinator.clients[&id].state == ClientState::Connected {
                        break;
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
                assert_eq!(acl.0.borrow()[Path::new("/direct")], BTreeSet::from([1000]));
            }
            assert!(coordinator.reconcile_topology(&moved, |_, _| {}).is_err());
            assert!(!coordinator.clients.contains_key(&id));
            assert!(coordinator.create_client(&source, 1000).is_err());
            assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
            open_device(&mut target_stream);
            assert_eq!(packet(&mut target_stream), protocol::SERVER_ERROR);
            fail.store(false, Ordering::SeqCst);
            coordinator.retry_access().unwrap();
            coordinator.reconcile_topology(&moved, |_, _| {}).unwrap();
            assert_eq!(acl.0.borrow()[Path::new("/direct")], BTreeSet::from([1001]));
            open_device(&mut target_stream);
            assert_eq!(packet(&mut target_stream), protocol::SERVER_DEVICE_OPENED);
            assert!(!coordinator.clients.contains_key(&id));
            drop(held);
            coordinator.shutdown().unwrap();
        }
    }

    #[test]
    fn shutdown_revokes_connections_and_slow_warning_boundary_is_testable() {
        let (mut coordinator, acl) = setup();
        let (_, fd) = coordinator.create_client(&seat(), 1000).unwrap();
        coordinator.shutdown().unwrap();
        let mut stream = std::os::unix::net::UnixStream::from(fd);
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        assert!(acl.0.borrow()[Path::new("/direct")].is_empty());
        assert!(!report_duration("test", Duration::from_millis(100)));
        assert!(report_duration("test", Duration::from_millis(101)));
    }
}
