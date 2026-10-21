use crate::authority::{AuthorizedDevice, DeviceAuthority};
use crate::device::DeviceOps;
use crate::protocol;
use crate::vt::{NoopVtOps, VtOps};
use seat_udev::SeatName;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;
use tokio::sync::mpsc;

pub type ClientId = u64;

#[derive(Debug)]
pub enum Outbound {
    Packet(Vec<u8>, Option<OwnedFd>),
    ArmDisableTimeout,
    Disconnect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientState {
    New,
    Active,
    PendingDisable,
    Disabled,
}

struct OpenDevice {
    id: i32,
    authorized: AuthorizedDevice,
    fd: OwnedFd,
    references: usize,
}

struct Client {
    seat: SeatName,
    session: i32,
    state: ClientState,
    tx: mpsc::UnboundedSender<Outbound>,
    devices: BTreeMap<i32, OpenDevice>,
}

struct Seat {
    vt_bound: bool,
    current_session: Option<i32>,
    next_session: i32,
    clients: VecDeque<ClientId>,
    active: Option<ClientId>,
    next: Option<ClientId>,
    /// A VT switch the kernel is holding open until the outgoing client
    /// has acknowledged its disable.
    release_pending: bool,
}

pub struct OpenedSeat {
    pub session: i32,
    pub active: bool,
}

pub struct OpenedDevice {
    pub id: i32,
    pub fd: OwnedFd,
}

pub struct Broker {
    authority: Rc<dyn DeviceAuthority>,
    device_ops: Rc<dyn DeviceOps>,
    vt_ops: Rc<dyn VtOps>,
    seats: HashMap<SeatName, Seat>,
    clients: HashMap<ClientId, Client>,
    next_client: ClientId,
}

impl Broker {
    pub fn new(authority: Rc<dyn DeviceAuthority>, device_ops: Rc<dyn DeviceOps>) -> Self {
        Self::with_vt(authority, device_ops, Rc::new(NoopVtOps))
    }

    pub fn with_vt(
        authority: Rc<dyn DeviceAuthority>,
        device_ops: Rc<dyn DeviceOps>,
        vt_ops: Rc<dyn VtOps>,
    ) -> Self {
        Self {
            authority,
            device_ops,
            vt_ops,
            seats: HashMap::new(),
            clients: HashMap::new(),
            next_client: 1,
        }
    }

    pub fn add_seat(&mut self, name: SeatName, vt_bound: bool, current_session: Option<i32>) {
        self.seats.entry(name).or_insert(Seat {
            vt_bound,
            current_session,
            next_session: 1,
            clients: VecDeque::new(),
            active: None,
            next: None,
            release_pending: false,
        });
    }

    pub fn remove_seat(&mut self, name: &SeatName) {
        let Some(seat) = self.seats.remove(name) else {
            return;
        };
        for id in seat.clients {
            if let Some(client) = self.clients.remove(&id) {
                let tx = client.tx.clone();
                self.revoke_all(client);
                let _ = tx.send(Outbound::Disconnect);
            }
        }
    }

    pub fn allocate_client(&mut self) -> ClientId {
        let id = self.next_client;
        self.next_client = self.next_client.wrapping_add(1).max(1);
        id
    }

    pub fn open_seat(
        &mut self,
        id: ClientId,
        name: &SeatName,
        tx: mpsc::UnboundedSender<Outbound>,
    ) -> Result<OpenedSeat, i32> {
        if self.clients.contains_key(&id) {
            return Err(libc::EBUSY);
        }
        self.refresh_current_session(name);
        let seat = self.seats.get_mut(name).ok_or(libc::ENOENT)?;
        if seat.vt_bound {
            if let Some(active) = seat.active {
                if self
                    .clients
                    .get(&active)
                    .map(|client| client.state != ClientState::PendingDisable)
                    .unwrap_or(false)
                {
                    return Err(libc::EBUSY);
                }
            }
        }
        let session = if seat.vt_bound {
            let session = seat.current_session.ok_or(libc::EINVAL)?;
            if seat.clients.iter().any(|candidate| {
                self.clients
                    .get(candidate)
                    .is_some_and(|client| client.session == session)
            }) {
                return Err(libc::EBUSY);
            }
            session
        } else {
            let session = seat.next_session;
            seat.next_session = seat.next_session.saturating_add(1);
            session
        };
        let active =
            seat.active.is_none() && (!seat.vt_bound || seat.current_session == Some(session));
        if active && seat.vt_bound {
            self.vt_ops.open(session).map_err(|error| {
                tracing::error!(seat = %name, vt = session, %error, "could not take over the VT");
                error.raw_os_error().unwrap_or(libc::EIO)
            })?;
            tracing::info!(seat = %name, vt = session, client = id, "took over the VT");
        }
        let state = if active {
            ClientState::Active
        } else {
            ClientState::New
        };
        seat.clients.push_back(id);
        if active {
            seat.active = Some(id);
        }
        self.clients.insert(
            id,
            Client {
                seat: name.clone(),
                session,
                state,
                tx,
                devices: BTreeMap::new(),
            },
        );
        tracing::info!(
            seat = %name,
            client = id,
            session,
            active,
            "opened seat"
        );
        Ok(OpenedSeat { session, active })
    }

    /// Re-reads the foreground VT of a VT-bound seat from the kernel.
    ///
    /// `current_session` is otherwise only written at startup and by the
    /// release/acquire handshake, which the daemon only receives for VTs it
    /// already owns. Anything else that switches VTs - a getty, `chvt`, or
    /// the display manager jumping to the session VT before its compositor
    /// connects - would leave the cache pointing at a terminal nobody is on,
    /// and the seat would then be arbitrated against that stale terminal.
    fn refresh_current_session(&mut self, name: &SeatName) {
        if !self.seats.get(name).is_some_and(|seat| seat.vt_bound) {
            return;
        }
        let current = match self.vt_ops.current() {
            Ok(vt) => vt,
            Err(error) => {
                tracing::warn!(seat = %name, %error, "could not read the foreground VT");
                return;
            }
        };
        let Some(seat) = self.seats.get_mut(name) else {
            return;
        };
        if seat.current_session != Some(current) {
            tracing::info!(
                seat = %name,
                previous = ?seat.current_session,
                current,
                "foreground VT changed outside the daemon"
            );
            seat.current_session = Some(current);
        }
    }

    pub fn close_client(&mut self, id: ClientId) {
        let Some(client) = self.clients.remove(&id) else {
            return;
        };
        let seat_name = client.seat.clone();
        let session = client.session;
        let mut closed_active_vt = false;
        if let Some(seat) = self.seats.get_mut(&seat_name) {
            seat.clients.retain(|candidate| *candidate != id);
            if seat.next == Some(id) {
                seat.next = None;
            }
            if seat.active == Some(id) {
                seat.active = None;
                closed_active_vt = seat.vt_bound;
            }
        }
        tracing::info!(seat = %seat_name, client = id, session, "closed client");
        self.revoke_all(client);
        self.activate_next(&seat_name);
        // A client that disconnects instead of acknowledging still has to
        // let the switch through.
        self.finish_vt_release(&seat_name);
        if closed_active_vt
            && self
                .seats
                .get(&seat_name)
                .is_some_and(|seat| seat.active.is_none())
        {
            if let Err(error) = self.vt_ops.close(session) {
                tracing::warn!(seat = %seat_name, vt = session, %error, "could not release the VT");
            } else {
                tracing::info!(seat = %seat_name, vt = session, "released the VT");
            }
        }
    }

    pub fn state(&self, id: ClientId) -> Option<ClientState> {
        self.clients.get(&id).map(|client| client.state)
    }

    pub fn open_device(&mut self, id: ClientId, requested: &Path) -> Result<OpenedDevice, i32> {
        let client = self.clients.get_mut(&id).ok_or(libc::ENOTCONN)?;
        if client.state != ClientState::Active {
            return Err(libc::EPERM);
        }
        let authorized = self
            .authority
            .authorize(&client.seat, requested)
            .map_err(|error| error.0)?;
        if let Some(existing) = client
            .devices
            .values_mut()
            .find(|device| device.authorized.devnum == authorized.devnum)
        {
            existing.references += 1;
            return Ok(OpenedDevice {
                id: existing.id,
                fd: rustix::io::dup(&existing.fd).map_err(|error| error.raw_os_error())?,
            });
        }
        if client.devices.len() >= 128 {
            return Err(libc::EMFILE);
        }
        let fd = self
            .device_ops
            .open(&authorized)
            .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO))?;
        let sent = rustix::io::dup(&fd).map_err(|error| error.raw_os_error())?;
        let device_id = client.devices.keys().next_back().copied().unwrap_or(0) + 1;
        client.devices.insert(
            device_id,
            OpenDevice {
                id: device_id,
                authorized,
                fd,
                references: 1,
            },
        );
        Ok(OpenedDevice {
            id: device_id,
            fd: sent,
        })
    }

    pub fn close_device(&mut self, id: ClientId, device_id: i32) -> Result<(), i32> {
        let client = self.clients.get_mut(&id).ok_or(libc::ENOTCONN)?;
        let device = client.devices.get_mut(&device_id).ok_or(libc::EBADF)?;
        device.references -= 1;
        if device.references == 0 {
            let device = client.devices.remove(&device_id).unwrap();
            let _ = self.device_ops.revoke(device.authorized.kind, &device.fd);
        }
        Ok(())
    }

    pub fn acknowledge_disable(&mut self, id: ClientId) -> Result<(), i32> {
        let seat_name = {
            let client = self.clients.get_mut(&id).ok_or(libc::ENOTCONN)?;
            if client.state != ClientState::PendingDisable {
                return Err(libc::EBUSY);
            }
            client.state = ClientState::Disabled;
            client.seat.clone()
        };
        if let Some(seat) = self.seats.get_mut(&seat_name) {
            if seat.active == Some(id) {
                seat.active = None;
            }
        }
        tracing::info!(client = id, seat = %seat_name, "client acknowledged the disable");
        self.activate_next(&seat_name);
        self.finish_vt_release(&seat_name);
        Ok(())
    }

    /// Completes a disable whose client failed to acknowledge in time. Device
    /// access was already revoked when disable began; this guarantees queue
    /// progress without trusting the client.
    pub fn force_disable(&mut self, id: ClientId) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if client.state != ClientState::PendingDisable {
            return;
        }
        client.state = ClientState::Disabled;
        let seat_name = client.seat.clone();
        tracing::warn!(client = id, seat = %seat_name, "client did not acknowledge the disable in time");
        if let Some(seat) = self.seats.get_mut(&seat_name) {
            if seat.active == Some(id) {
                seat.active = None;
            }
        }
        self.activate_next(&seat_name);
        self.finish_vt_release(&seat_name);
    }

    pub fn switch_session(&mut self, id: ClientId, session: i32) -> Result<(), i32> {
        if session <= 0 {
            return Err(libc::EINVAL);
        }
        let seat_name = self
            .clients
            .get(&id)
            .filter(|client| client.state == ClientState::Active)
            .map(|client| client.seat.clone())
            .ok_or(libc::EPERM)?;
        let seat = self.seats.get_mut(&seat_name).ok_or(libc::ENOENT)?;
        if seat.vt_bound {
            let current = self.clients.get(&id).unwrap().session;
            return self
                .vt_ops
                .switch(current, session)
                .map_err(|error| error.raw_os_error().unwrap_or(libc::EIO));
        }
        let target = seat
            .clients
            .iter()
            .copied()
            .find(|candidate| self.clients.get(candidate).map(|c| c.session) == Some(session))
            .ok_or(libc::EINVAL)?;
        if target == id {
            return Ok(());
        }
        if seat.next.is_some() {
            return Err(libc::EBUSY);
        }
        seat.next = Some(target);
        self.disable_client(id)
    }

    /// Starts the release handshake for a VT switch the kernel has signalled.
    ///
    /// The kernel is only answered once the outgoing client reports that it
    /// has given up its devices, or once the disable is forced. Answering
    /// earlier releases `VT_WAITACTIVE` in whatever asked for the switch,
    /// which then starts the next compositor while this seat still has an
    /// active client; that compositor is refused its devices and gives up,
    /// which is exactly how a display manager's session dies with no
    /// diagnosis beyond "no drm device found".
    pub fn vt_release(&mut self, name: &SeatName) {
        let active = self.seats.get(name).and_then(|seat| seat.active);
        tracing::info!(seat = %name, client = ?active, "VT release requested");
        let mut disabled = false;
        if let Some(id) = active {
            match self.disable_client(id) {
                Ok(()) => disabled = true,
                Err(error) => tracing::warn!(
                    seat = %name,
                    client = id,
                    errno = error,
                    "could not disable the active client for the VT release"
                ),
            }
        }
        if let Some(seat) = self.seats.get_mut(name) {
            seat.current_session = None;
            seat.release_pending = true;
        }
        if !disabled {
            self.finish_vt_release(name);
        }
    }

    /// Answers a pending VT release once no client still owes an
    /// acknowledgement.
    fn finish_vt_release(&mut self, name: &SeatName) {
        let Some(seat) = self.seats.get(name) else {
            return;
        };
        if !seat.release_pending {
            return;
        }
        if seat.clients.iter().any(|id| {
            self.clients
                .get(id)
                .is_some_and(|client| client.state == ClientState::PendingDisable)
        }) {
            return;
        }
        if let Some(seat) = self.seats.get_mut(name) {
            seat.release_pending = false;
        }
        match self.vt_ops.ack_release() {
            Ok(()) => tracing::info!(seat = %name, "acknowledged the VT release"),
            Err(error) => {
                tracing::warn!(seat = %name, %error, "could not acknowledge the VT release")
            }
        }
    }

    pub fn vt_acquire(&mut self, name: &SeatName, session: i32) {
        tracing::info!(seat = %name, vt = session, "VT acquired");
        if let Some(seat) = self.seats.get_mut(name) {
            seat.current_session = Some(session);
        }
        self.activate_next(name);
    }

    pub fn revoke_path(&mut self, seat: &SeatName, path: &Path) {
        let ids = self
            .seats
            .get(seat)
            .map(|state| state.clients.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for id in ids {
            let Some(client) = self.clients.get_mut(&id) else {
                continue;
            };
            let remove = client
                .devices
                .iter()
                .filter(|(_, device)| device.authorized.path == path)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            for device_id in remove {
                if let Some(device) = client.devices.remove(&device_id) {
                    let _ = self.device_ops.revoke(device.authorized.kind, &device.fd);
                }
            }
        }
    }

    fn disable_client(&mut self, id: ClientId) -> Result<(), i32> {
        let client = self.clients.get_mut(&id).ok_or(libc::ENOTCONN)?;
        if client.state != ClientState::Active {
            return Err(libc::EBUSY);
        }
        for device in client.devices.values() {
            let _ = self.device_ops.revoke(device.authorized.kind, &device.fd);
        }
        client.devices.clear();
        client.state = ClientState::PendingDisable;
        tracing::info!(client = id, seat = %client.seat, "disabling client");
        client
            .tx
            .send(Outbound::Packet(
                protocol::empty(protocol::SERVER_DISABLE_SEAT),
                None,
            ))
            .map_err(|_| libc::EPIPE)?;
        client
            .tx
            .send(Outbound::ArmDisableTimeout)
            .map_err(|_| libc::EPIPE)
    }

    fn activate_next(&mut self, name: &SeatName) {
        let candidate = {
            let Some(seat) = self.seats.get_mut(name) else {
                return;
            };
            if seat.active.is_some() {
                return;
            }
            seat.next.take().or_else(|| {
                seat.clients.iter().copied().find(|id| {
                    self.clients
                        .get(id)
                        .map(|client| {
                            matches!(client.state, ClientState::New | ClientState::Disabled)
                                && (!seat.vt_bound || seat.current_session == Some(client.session))
                        })
                        .unwrap_or(false)
                })
            })
        };
        let Some(id) = candidate else {
            return;
        };
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if self.seats.get(name).is_some_and(|seat| seat.vt_bound) {
            if let Err(error) = self.vt_ops.open(client.session) {
                tracing::error!(
                    seat = %name,
                    client = id,
                    vt = client.session,
                    %error,
                    "could not take over the VT; leaving the seat inactive"
                );
                return;
            }
        }
        tracing::info!(seat = %name, client = id, session = client.session, "activated client");
        client.state = ClientState::Active;
        if let Some(seat) = self.seats.get_mut(name) {
            seat.active = Some(id);
        }
        if client
            .tx
            .send(Outbound::Packet(
                protocol::empty(protocol::SERVER_ENABLE_SEAT),
                None,
            ))
            .is_err()
        {
            self.close_client(id);
        }
    }

    fn revoke_all(&self, mut client: Client) {
        for (_, device) in std::mem::take(&mut client.devices) {
            let _ = self.device_ops.revoke(device.authorized.kind, &device.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::OpenError;
    use crate::vt::VtOps;
    use seat_udev::DeviceKind;
    use std::cell::{Cell, RefCell};
    use std::fs::File;
    use std::io;
    use std::os::fd::IntoRawFd;
    use std::os::fd::{FromRawFd, OwnedFd};

    struct FakeAuthority;

    impl DeviceAuthority for FakeAuthority {
        fn authorize(
            &self,
            seat: &SeatName,
            requested: &Path,
        ) -> Result<AuthorizedDevice, OpenError> {
            if seat.as_str() != "seat1" {
                return Err(OpenError(libc::EPERM));
            }
            Ok(AuthorizedDevice {
                path: requested.into(),
                devnum: 7,
                kind: DeviceKind::Evdev,
            })
        }
    }

    #[derive(Default)]
    struct FakeOps {
        revoked: Cell<usize>,
    }

    impl DeviceOps for FakeOps {
        fn open(&self, _device: &AuthorizedDevice) -> io::Result<OwnedFd> {
            let raw = File::open("/dev/null")?.into_raw_fd();
            // SAFETY: ownership was transferred by into_raw_fd.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }

        fn revoke(&self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
            self.revoked.set(self.revoked.get() + 1);
            Ok(())
        }
    }

    fn broker() -> (Broker, Rc<FakeOps>) {
        let ops = Rc::new(FakeOps::default());
        let mut broker = Broker::new(Rc::new(FakeAuthority), ops.clone());
        broker.add_seat(SeatName::parse("seat1").unwrap(), false, None);
        (broker, ops)
    }

    #[derive(Default)]
    struct FakeVt {
        calls: RefCell<Vec<String>>,
        /// `None` makes the kernel query fail, which keeps the seat on its
        /// cached VT the way the daemon behaves on a system without VTs.
        current: Cell<Option<i32>>,
    }

    impl VtOps for FakeVt {
        fn current(&self) -> io::Result<i32> {
            self.current
                .get()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOTSUP))
        }

        fn open(&self, vt: i32) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("open:{vt}"));
            Ok(())
        }
        fn close(&self, vt: i32) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("close:{vt}"));
            Ok(())
        }
        fn switch(&self, _current: i32, target: i32) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("switch:{target}"));
            Ok(())
        }
        fn ack_release(&self) -> io::Result<()> {
            self.calls.borrow_mut().push("ack_release".to_string());
            Ok(())
        }
        fn ack_acquire(&self) -> io::Result<()> {
            self.calls.borrow_mut().push("ack_acquire".to_string());
            Ok(())
        }
    }

    #[test]
    fn the_kernel_release_is_answered_only_after_the_client_acknowledges() {
        // The kernel holds the switching process inside VT_WAITACTIVE until
        // the release is answered. Answering it before the outgoing client
        // has given up its devices lets a display manager start the next
        // compositor against a seat that is still busy.
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = broker.allocate_client();
        assert!(broker.open_seat(client, &seat, tx).unwrap().active);

        broker.vt_release(&seat);
        assert_eq!(vt.calls.borrow().as_slice(), ["open:7"]);

        broker.acknowledge_disable(client).unwrap();
        assert_eq!(vt.calls.borrow().as_slice(), ["open:7", "ack_release"]);
    }

    #[test]
    fn a_forced_disable_still_answers_the_kernel_release() {
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = broker.allocate_client();
        broker.open_seat(client, &seat, tx).unwrap();
        broker.vt_release(&seat);
        broker.force_disable(client);
        assert!(vt.calls.borrow().contains(&"ack_release".to_string()));
    }

    #[test]
    fn a_client_that_dies_during_a_release_does_not_wedge_the_switch() {
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = broker.allocate_client();
        broker.open_seat(client, &seat, tx).unwrap();
        broker.vt_release(&seat);
        broker.close_client(client);
        assert!(vt.calls.borrow().contains(&"ack_release".to_string()));
    }

    #[test]
    fn a_release_with_no_active_client_is_answered_at_once() {
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        broker.vt_release(&seat);
        assert_eq!(vt.calls.borrow().as_slice(), ["ack_release"]);
    }

    #[test]
    fn the_next_compositor_becomes_active_once_the_previous_one_is_gone() {
        // The sequence a display manager produces: the greeter is disabled
        // by the switch, the kernel completes it, and the session's
        // compositor connects on the new VT and must be able to use it.
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (greeter_tx, _greeter_rx) = mpsc::unbounded_channel();
        let (session_tx, _session_rx) = mpsc::unbounded_channel();
        let greeter = broker.allocate_client();
        let session = broker.allocate_client();
        broker.open_seat(greeter, &seat, greeter_tx).unwrap();

        vt.current.set(Some(7));
        broker.vt_release(&seat);
        broker.acknowledge_disable(greeter).unwrap();

        // Only now does the kernel put VT 8 in front and let the session's
        // compositor run.
        vt.current.set(Some(8));
        let opened = broker.open_seat(session, &seat, session_tx).unwrap();
        assert_eq!(opened.session, 8);
        assert!(opened.active);
        assert_eq!(broker.state(session), Some(ClientState::Active));
    }

    #[test]
    fn vt_admission_waits_for_release_and_activates_the_acquired_vt() {
        let vt = Rc::new(FakeVt::default());
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let first = broker.allocate_client();
        let second = broker.allocate_client();
        assert!(broker.open_seat(first, &seat, tx1.clone()).unwrap().active);
        assert!(matches!(
            broker.open_seat(second, &seat, tx2.clone()),
            Err(libc::EBUSY)
        ));
        broker.vt_release(&seat);
        broker.vt_acquire(&seat, 8);
        assert!(!broker.open_seat(second, &seat, tx2).unwrap().active);
        broker.acknowledge_disable(first).unwrap();
        assert_eq!(broker.state(second), Some(ClientState::Active));
        assert!(matches!(rx2.try_recv(), Ok(Outbound::Packet(_, None))));
        // The kernel is answered last, after the outgoing client reports it
        // has let go.
        assert_eq!(
            vt.calls.borrow().as_slice(),
            ["open:7", "open:8", "ack_release"]
        );
    }

    #[test]
    fn open_seat_prefers_the_kernel_vt_over_the_cached_one() {
        // The daemon started while VT 7 was in front, then something it does
        // not arbitrate - a getty, chvt, or a display manager jumping to the
        // session VT before its compositor connects - moved the foreground
        // to VT 1. The client must be bound to VT 1, and VT 1 is the
        // terminal whose switches the daemon must take over.
        let vt = Rc::new(FakeVt::default());
        vt.current.set(Some(1));
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::seat0();
        broker.add_seat(seat.clone(), true, Some(7));
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = broker.allocate_client();
        let opened = broker.open_seat(client, &seat, tx).unwrap();
        assert_eq!(opened.session, 1);
        assert!(opened.active);
        assert_eq!(vt.calls.borrow().as_slice(), ["open:1"]);

        // Closing the client releases the VT it actually held.
        broker.close_client(client);
        assert_eq!(vt.calls.borrow().as_slice(), ["open:1", "close:1"]);
    }

    #[test]
    fn non_vt_seats_never_consult_the_kernel_vt() {
        let vt = Rc::new(FakeVt::default());
        vt.current.set(Some(1));
        let mut broker = Broker::with_vt(
            Rc::new(FakeAuthority),
            Rc::new(FakeOps::default()),
            vt.clone(),
        );
        let seat = SeatName::parse("seat1").unwrap();
        broker.add_seat(seat.clone(), false, None);
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = broker.allocate_client();
        let opened = broker.open_seat(client, &seat, tx).unwrap();
        assert_eq!(opened.session, 1);
        assert!(opened.active);
        assert!(vt.calls.borrow().is_empty());
    }

    #[test]
    fn non_vt_clients_queue_and_activate_after_removal() {
        let (mut broker, _) = broker();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let first = broker.allocate_client();
        let second = broker.allocate_client();
        assert!(
            broker
                .open_seat(first, &SeatName::parse("seat1").unwrap(), tx1)
                .unwrap()
                .active
        );
        assert!(
            !broker
                .open_seat(second, &SeatName::parse("seat1").unwrap(), tx2)
                .unwrap()
                .active
        );
        assert_eq!(broker.state(second), Some(ClientState::New));
        broker.close_client(first);
        assert_eq!(broker.state(second), Some(ClientState::Active));
        assert!(matches!(
            rx2.try_recv().unwrap(),
            Outbound::Packet(bytes, None) if bytes == protocol::empty(protocol::SERVER_ENABLE_SEAT)
        ));
    }

    #[test]
    fn inactive_clients_cannot_open_and_revocation_drops_active_devices() {
        let (mut broker, ops) = broker();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let first = broker.allocate_client();
        let second = broker.allocate_client();
        let seat = SeatName::parse("seat1").unwrap();
        broker.open_seat(first, &seat, tx1).unwrap();
        broker.open_seat(second, &seat, tx2).unwrap();
        assert!(matches!(
            broker.open_device(second, Path::new("/dev/input/event0")),
            Err(libc::EPERM)
        ));
        broker
            .open_device(first, Path::new("/dev/input/event0"))
            .unwrap();
        broker.revoke_path(&seat, Path::new("/dev/input/event0"));
        assert_eq!(ops.revoked.get(), 1);
    }

    #[test]
    fn forced_disable_makes_queue_progress_without_an_ack() {
        let (mut broker, _) = broker();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let first = broker.allocate_client();
        let second = broker.allocate_client();
        let seat = SeatName::parse("seat1").unwrap();
        broker.open_seat(first, &seat, tx1).unwrap();
        let target_session = broker.open_seat(second, &seat, tx2).unwrap().session;
        broker.switch_session(first, target_session).unwrap();
        broker.force_disable(first);
        assert_eq!(broker.state(first), Some(ClientState::Disabled));
        assert_eq!(broker.state(second), Some(ClientState::Active));
        assert!(matches!(rx2.try_recv(), Ok(Outbound::Packet(_, None))));
    }

    #[test]
    fn switch_waits_for_disable_ack_before_enabling_target() {
        let (mut broker, _) = broker();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let first = broker.allocate_client();
        let second = broker.allocate_client();
        let seat = SeatName::parse("seat1").unwrap();
        broker.open_seat(first, &seat, tx1).unwrap();
        let target_session = broker.open_seat(second, &seat, tx2).unwrap().session;
        broker.switch_session(first, target_session).unwrap();
        assert_eq!(broker.state(first), Some(ClientState::PendingDisable));
        assert!(matches!(rx1.try_recv(), Ok(Outbound::Packet(_, None))));
        assert!(rx2.try_recv().is_err());
        broker.acknowledge_disable(first).unwrap();
        assert_eq!(broker.state(second), Some(ClientState::Active));
        assert!(matches!(rx2.try_recv(), Ok(Outbound::Packet(_, None))));
    }
}
