use crate::device::{AllowedDevice, DeviceId, DeviceKind, DeviceOps};
use crate::protocol::{self, Packet};
use crate::vt::VtOps;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use thiserror::Error;

const MAX_OPEN_DEVICES: usize = 128;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(String);

impl ClientId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientState {
    Connected,
    Waiting,
    Active,
    PendingDisable,
    Disabled,
}

#[derive(Debug)]
pub enum Action {
    Send {
        client: ClientId,
        bytes: Vec<u8>,
        fd: Option<OwnedFd>,
    },
    StateChanged {
        client: ClientId,
        state: ClientState,
    },
    ClientGone(ClientId),
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("client {0} already exists")]
    DuplicateClient(ClientId),
    #[error("client {0} does not exist")]
    UnknownClient(ClientId),
    #[error("device {0} already exists")]
    DuplicateDevice(DeviceId),
    #[error("device {0} does not exist")]
    UnknownDevice(DeviceId),
    #[error("seatd protocol violation by client {0}")]
    Protocol(ClientId),
    #[error("device operation failed: {0}")]
    Device(#[source] io::Error),
    #[error("VT operation failed: {0}")]
    Vt(#[source] io::Error),
}

struct AuthorizedDevice {
    device: AllowedDevice,
    generation: u64,
}

struct OpenDevice {
    protocol_id: i32,
    authorization_id: DeviceId,
    generation: u64,
    kind: DeviceKind,
    fd: OwnedFd,
    references: usize,
    active: bool,
    revoked: bool,
}

struct Client {
    state: ClientState,
    session: Option<i32>,
    devices: BTreeMap<i32, OpenDevice>,
}

/// Pure one-seat protocol and arbitration state.
///
/// Socket polling is deliberately outside this type, allowing deterministic
/// protocol, device and session tests without threads or physical devices.
pub struct SeatState<D: DeviceOps> {
    name: String,
    device_ops: D,
    allowed: BTreeMap<DeviceId, AuthorizedDevice>,
    next_generation: u64,
    clients: BTreeMap<ClientId, Client>,
    members: VecDeque<ClientId>,
    active: Option<ClientId>,
    next: Option<ClientId>,
    vt: Option<Box<dyn VtOps>>,
    current_vt: i32,
    vt_release_pending: bool,
}

impl<D: DeviceOps> SeatState<D> {
    pub fn new(name: impl Into<String>, device_ops: D) -> Self {
        Self::with_vt(name, device_ops, None)
    }

    pub(crate) fn new_vt(name: impl Into<String>, device_ops: D, vt_ops: Box<dyn VtOps>) -> Self {
        Self::with_vt(name, device_ops, Some(vt_ops))
    }

    fn with_vt(name: impl Into<String>, device_ops: D, vt: Option<Box<dyn VtOps>>) -> Self {
        Self {
            name: name.into(),
            device_ops,
            allowed: BTreeMap::new(),
            next_generation: 1,
            clients: BTreeMap::new(),
            members: VecDeque::new(),
            active: None,
            next: None,
            vt,
            current_vt: -1,
            vt_release_pending: false,
        }
    }

    pub fn add_client(&mut self, id: ClientId) -> Result<(), StateError> {
        if self.clients.contains_key(&id) {
            return Err(StateError::DuplicateClient(id));
        }
        self.clients.insert(
            id,
            Client {
                state: ClientState::Connected,
                session: None,
                devices: BTreeMap::new(),
            },
        );
        Ok(())
    }

    pub fn client_ids(&self) -> Vec<ClientId> {
        self.clients.keys().cloned().collect()
    }

    pub fn client_state(&self, id: &ClientId) -> Option<ClientState> {
        self.clients.get(id).map(|client| client.state)
    }

    pub fn client_session(&self, id: &ClientId) -> Option<i32> {
        self.clients.get(id).and_then(|client| client.session)
    }

    pub fn vt_bound(&self) -> bool {
        self.vt.is_some()
    }

    pub fn vt_release_pending(&self) -> bool {
        self.vt_release_pending
    }

    pub fn handle_vt_release(&mut self) -> Result<Vec<Action>, StateError> {
        if self.vt.is_none() || self.vt_release_pending {
            return Ok(Vec::new());
        }
        self.refresh_current_vt()?;
        self.vt_release_pending = true;
        let Some(active) = self.active.clone() else {
            self.finish_vt_release()?;
            return Ok(Vec::new());
        };
        if self.clients[&active].state == ClientState::Active {
            Ok(self.disable_active(&active))
        } else {
            Ok(Vec::new())
        }
    }

    pub fn handle_vt_acquire(&mut self) -> Result<Vec<Action>, StateError> {
        if self.vt.is_none() {
            return Ok(Vec::new());
        }
        if self.vt_release_pending {
            tracing::warn!("ignoring VT acquire while release is still pending");
            return Ok(Vec::new());
        }
        self.refresh_current_vt()?;
        self.vt
            .as_mut()
            .expect("VT-bound state has VT operations")
            .ack_acquire()
            .map_err(StateError::Vt)?;
        Ok(self.activate_next())
    }

    pub fn handle_vt_release_timeout(&mut self) -> Result<Vec<Action>, StateError> {
        if !self.vt_release_pending {
            return Ok(Vec::new());
        }
        let mut actions = Vec::new();
        if let Some(active) = self.active.take() {
            if let Some(client) = self.clients.get_mut(&active) {
                client.state = ClientState::Disabled;
                actions.push(Action::StateChanged {
                    client: active,
                    state: ClientState::Disabled,
                });
            }
        }
        self.finish_vt_release()?;
        Ok(actions)
    }

    pub fn add_device(&mut self, mut device: AllowedDevice) -> Result<Vec<Action>, StateError> {
        if self.allowed.contains_key(&device.id) {
            return Err(StateError::DuplicateDevice(device.id));
        }
        device.path = fs::canonicalize(&device.path).map_err(StateError::Device)?;
        if self
            .allowed
            .values()
            .any(|old| old.device.path == device.path)
        {
            return Err(StateError::Device(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multiple device IDs authorize the same canonical path",
            )));
        }
        if self.has_pending_revocations() {
            return Err(StateError::Device(io::Error::from_raw_os_error(
                libc::EBUSY,
            )));
        }
        let generation = self.allocate_generation();
        self.allowed
            .insert(device.id.clone(), AuthorizedDevice { device, generation });
        Ok(Vec::new())
    }

    pub fn replace_devices(
        &mut self,
        devices: Vec<AllowedDevice>,
    ) -> Result<Vec<Action>, StateError> {
        // Validate the complete command before changing any authorization.
        let mut replacements = BTreeMap::new();
        let mut paths = BTreeSet::new();
        for mut device in devices {
            device.path = fs::canonicalize(&device.path).map_err(StateError::Device)?;
            if replacements.contains_key(&device.id) {
                return Err(StateError::DuplicateDevice(device.id));
            }
            if !paths.insert(device.path.clone()) {
                return Err(StateError::Device(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multiple device IDs authorize the same canonical path",
                )));
            }
            replacements.insert(device.id.clone(), device);
        }
        let removed = self
            .allowed
            .iter()
            .filter(|(id, old)| replacements.get(*id) != Some(&old.device))
            .map(|(id, old)| (id.clone(), old.generation))
            .collect::<Vec<_>>();

        let mut revoked = removed.into_iter().collect::<BTreeSet<_>>();
        for (id, _) in &revoked {
            self.allowed.remove(id);
        }
        // A failed ioctl must remain retryable even after its authorization
        // has been removed. Do not authorize any replacements until all old
        // privileged uses have been deactivated successfully.
        for client in self.clients.values() {
            for device in client
                .devices
                .values()
                .filter(|device| device.revoked && device.active)
            {
                revoked.insert((device.authorization_id.clone(), device.generation));
            }
        }
        let mut first_error = None;
        for (id, generation) in revoked {
            if let Err(error) = self.revoke_authorization(&id, generation) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(StateError::Device(error));
        }

        let mut next = BTreeMap::new();
        for (id, device) in replacements {
            let generation = self
                .allowed
                .get(&id)
                .filter(|old| old.device == device)
                .map(|old| old.generation)
                .unwrap_or_else(|| self.allocate_generation());
            next.insert(id, AuthorizedDevice { device, generation });
        }
        self.allowed = next;
        Ok(Vec::new())
    }

    /// Deny new opens immediately and retry any failed revocation for this ID.
    /// Removing an absent ID is idempotent.
    pub fn remove_device(&mut self, id: &DeviceId) -> Result<Vec<Action>, StateError> {
        self.allowed.remove(id);
        let generations = self
            .clients
            .values()
            .flat_map(|client| {
                client
                    .devices
                    .values()
                    .filter(|device| device.authorization_id == *id)
                    .map(|device| device.generation)
            })
            .collect::<BTreeSet<_>>();
        let mut first_error = None;
        for generation in generations {
            if let Err(error) = self.revoke_authorization(id, generation) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or_else(|| Ok(Vec::new()), |error| Err(StateError::Device(error)))
    }

    pub fn revoke_client(&mut self, id: &ClientId) -> Result<Vec<Action>, StateError> {
        self.remove_client(id, true)
    }

    pub fn handle_eof(&mut self, id: &ClientId) -> Result<Vec<Action>, StateError> {
        self.remove_client(id, true)
    }

    pub fn handle_packet(
        &mut self,
        id: &ClientId,
        packet: Packet,
    ) -> Result<Vec<Action>, StateError> {
        if !self.clients.contains_key(id) {
            return Err(StateError::UnknownClient(id.clone()));
        }
        match packet.opcode {
            protocol::CLIENT_OPEN_SEAT => {
                protocol::parse_empty(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.open_seat(id)
            }
            protocol::CLIENT_CLOSE_SEAT => {
                protocol::parse_empty(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.close_seat(id)
            }
            protocol::CLIENT_OPEN_DEVICE => {
                let path = protocol::parse_path(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.require_membership(id)?;
                Ok(self.open_device(id, &path))
            }
            protocol::CLIENT_CLOSE_DEVICE => {
                let device_id = protocol::parse_i32(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.require_membership(id)?;
                Ok(self.close_device(id, device_id))
            }
            protocol::CLIENT_DISABLE_SEAT => {
                protocol::parse_empty(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.require_membership(id)?;
                Ok(self.acknowledge_disable(id))
            }
            protocol::CLIENT_SWITCH_SESSION => {
                let session = protocol::parse_i32(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                self.require_membership(id)?;
                Ok(self.switch_session(id, session))
            }
            protocol::CLIENT_PING => {
                protocol::parse_empty(&packet.payload)
                    .map_err(|_| StateError::Protocol(id.clone()))?;
                Ok(vec![send(id, protocol::empty(protocol::SERVER_PONG), None)])
            }
            _ => Err(StateError::Protocol(id.clone())),
        }
    }

    fn require_membership(&self, id: &ClientId) -> Result<(), StateError> {
        if self.clients[id].state == ClientState::Connected {
            Err(StateError::Protocol(id.clone()))
        } else {
            Ok(())
        }
    }

    fn open_seat(&mut self, id: &ClientId) -> Result<Vec<Action>, StateError> {
        if self.clients.get(id).map(|client| client.state) != Some(ClientState::Connected) {
            return Err(StateError::Protocol(id.clone()));
        }
        // Validate the response before changing membership or arbitration.
        // A bad seat configuration must not leave an invisible active client.
        let opened = protocol::seat_opened(&self.name).map_err(StateError::Device)?;
        let session = if self.vt.is_some() {
            self.refresh_current_vt()?;
            if self.current_vt <= 0
                || (self.active.is_some()
                    && self.clients[self.active.as_ref().expect("active client exists")].state
                        != ClientState::PendingDisable)
                || self.members.iter().any(|member| {
                    self.clients.get(member).and_then(|client| client.session)
                        == Some(self.current_vt)
                })
            {
                return Err(StateError::Protocol(id.clone()));
            }
            self.current_vt
        } else {
            self.lowest_free_session()
        };

        let client = self.clients.get_mut(id).expect("client existence checked");
        client.session = Some(session);
        client.state = ClientState::Waiting;
        self.members.push_back(id.clone());

        let mut actions = vec![send(id, opened, None)];
        actions.push(Action::StateChanged {
            client: id.clone(),
            state: ClientState::Waiting,
        });
        if self.active.is_none() && !self.vt_release_pending {
            actions.extend(self.activate_client(id));
        }
        Ok(actions)
    }

    fn close_seat(&mut self, id: &ClientId) -> Result<Vec<Action>, StateError> {
        if self.clients.get(id).map(|client| client.state) == Some(ClientState::Connected) {
            return Err(StateError::Protocol(id.clone()));
        }
        let mut actions = self.remove_membership(id)?;
        actions.push(send(
            id,
            protocol::empty(protocol::SERVER_SEAT_CLOSED),
            None,
        ));
        actions.push(Action::StateChanged {
            client: id.clone(),
            state: ClientState::Connected,
        });
        Ok(actions)
    }

    fn remove_client(
        &mut self,
        id: &ClientId,
        report_gone: bool,
    ) -> Result<Vec<Action>, StateError> {
        if !self.clients.contains_key(id) {
            return Ok(Vec::new());
        }
        let mut actions = if self.clients[id].state == ClientState::Connected {
            Vec::new()
        } else {
            // Cleanup must complete even if a VT acknowledgement ioctl fails;
            // otherwise an explicit revoke or EOF could leave a half-removed
            // client and a live connection behind.
            match self.remove_membership(id) {
                Ok(actions) => actions,
                Err(error) => {
                    tracing::error!(client = %id, %error, "continuing client removal after cleanup error");
                    Vec::new()
                }
            }
        };
        self.clients.remove(id);
        if report_gone {
            actions.push(Action::ClientGone(id.clone()));
        }
        Ok(actions)
    }

    fn remove_membership(&mut self, id: &ClientId) -> Result<Vec<Action>, StateError> {
        let was_active = self.active.as_ref() == Some(id);
        if self.next.as_ref() == Some(id) {
            self.next = None;
        }
        self.members.retain(|candidate| candidate != id);
        let client = self
            .clients
            .get_mut(id)
            .ok_or_else(|| StateError::UnknownClient(id.clone()))?;
        let session = client.session;
        for device in client.devices.values_mut() {
            if device.active {
                if let Err(error) = self.device_ops.deactivate(device.kind, &device.fd) {
                    tracing::warn!(client = %id, device = device.protocol_id, %error, "could not deactivate device while closing seat");
                }
                device.active = false;
            }
        }
        client.devices.clear();
        client.session = None;
        client.state = ClientState::Connected;
        if was_active {
            self.active = None;
        }

        let mut actions = Vec::new();
        if was_active {
            if self.vt_release_pending {
                self.finish_vt_release()?;
            } else {
                actions.extend(self.activate_next());
            }
        }
        if let (Some(vt), Some(session)) = (self.vt.as_mut(), session)
            && (!was_active || self.active.is_none())
            && let Err(error) = vt.close(session)
        {
            tracing::warn!(session, %error, "could not restore departing client's VT");
        }
        Ok(actions)
    }

    fn open_device(&mut self, id: &ClientId, requested: &Path) -> Vec<Action> {
        if self.clients[id].state != ClientState::Active {
            return vec![send_error(id, libc::EPERM)];
        }
        let canonical = match fs::canonicalize(requested) {
            Ok(path) => path,
            Err(error) => {
                return vec![send_error(id, error.raw_os_error().unwrap_or(libc::ENOENT))];
            }
        };
        let Some(authorized) = self
            .allowed
            .values()
            .find(|authorized| authorized.device.path == canonical)
        else {
            return vec![send_error(id, libc::EPERM)];
        };
        let authorization_id = authorized.device.id.clone();
        let generation = authorized.generation;
        let kind = authorized.device.kind;

        if let Some(existing) = self.clients.get_mut(id).and_then(|client| {
            client.devices.values_mut().find(|open| {
                !open.revoked
                    && open.authorization_id == authorization_id
                    && open.generation == generation
            })
        }) {
            let fd = match rustix::io::dup(&existing.fd) {
                Ok(fd) => fd,
                Err(error) => return vec![send_error(id, error.raw_os_error())],
            };
            existing.references += 1;
            return vec![send(
                id,
                protocol::device_opened(existing.protocol_id),
                Some(fd),
            )];
        }

        if self.clients[id].devices.len() >= MAX_OPEN_DEVICES {
            return vec![send_error(id, libc::EMFILE)];
        }
        let fd = match self.device_ops.open(&authorized.device) {
            Ok(fd) => fd,
            Err(error) => {
                return vec![send_error(id, error.raw_os_error().unwrap_or(libc::EIO))];
            }
        };
        let sent = match rustix::io::dup(&fd) {
            Ok(fd) => fd,
            Err(error) => return vec![send_error(id, error.raw_os_error())],
        };
        let protocol_id = lowest_free_device_id(&self.clients[id].devices);
        self.clients.get_mut(id).unwrap().devices.insert(
            protocol_id,
            OpenDevice {
                protocol_id,
                authorization_id,
                generation,
                kind,
                fd,
                references: 1,
                active: true,
                revoked: false,
            },
        );
        vec![send(id, protocol::device_opened(protocol_id), Some(sent))]
    }

    fn close_device(&mut self, id: &ClientId, protocol_id: i32) -> Vec<Action> {
        if self.clients[id].state == ClientState::Connected {
            return vec![send_error(id, libc::ENOTCONN)];
        }
        let Some(device) = self
            .clients
            .get_mut(id)
            .unwrap()
            .devices
            .get_mut(&protocol_id)
        else {
            return vec![send_error(id, libc::EBADF)];
        };
        device.references -= 1;
        if device.references == 0 {
            let device = self
                .clients
                .get_mut(id)
                .unwrap()
                .devices
                .remove(&protocol_id)
                .unwrap();
            if device.active {
                let _ = self.device_ops.deactivate(device.kind, &device.fd);
            }
        }
        vec![send(
            id,
            protocol::empty(protocol::SERVER_DEVICE_CLOSED),
            None,
        )]
    }

    fn switch_session(&mut self, id: &ClientId, session: i32) -> Vec<Action> {
        if self.clients[id].state != ClientState::Active {
            return vec![send_error(id, libc::EPERM)];
        }
        if session <= 0 {
            return vec![send_error(id, libc::EINVAL)];
        }
        if self.clients[id].session == Some(session) {
            return vec![send(
                id,
                protocol::empty(protocol::SERVER_SESSION_SWITCHED),
                None,
            )];
        }
        if let Some(vt) = self.vt.as_mut() {
            let current = match vt.current() {
                Ok(current) => current,
                Err(error) => {
                    return vec![send_error(id, error.raw_os_error().unwrap_or(libc::EIO))];
                }
            };
            self.current_vt = current;
            if let Err(error) = vt.switch(current, session) {
                return vec![send_error(id, error.raw_os_error().unwrap_or(libc::EIO))];
            }
            return vec![send(
                id,
                protocol::empty(protocol::SERVER_SESSION_SWITCHED),
                None,
            )];
        }

        if self.next.is_some() {
            return vec![send(
                id,
                protocol::empty(protocol::SERVER_SESSION_SWITCHED),
                None,
            )];
        }
        let target = self.members.iter().find(|candidate| {
            self.clients
                .get(*candidate)
                .and_then(|client| client.session)
                == Some(session)
        });
        let Some(target) = target.cloned() else {
            return vec![send_error(id, libc::EINVAL)];
        };
        self.next = Some(target);
        let mut actions = self.disable_active(id);
        actions.push(send(
            id,
            protocol::empty(protocol::SERVER_SESSION_SWITCHED),
            None,
        ));
        actions
    }

    fn disable_active(&mut self, id: &ClientId) -> Vec<Action> {
        let client = self.clients.get_mut(id).unwrap();
        for device in client.devices.values_mut() {
            if device.active {
                match self.device_ops.deactivate(device.kind, &device.fd) {
                    Ok(()) => device.active = false,
                    Err(error) => {
                        tracing::warn!(client = %id, device = device.protocol_id, %error, "could not deactivate device");
                    }
                }
            }
        }
        client.state = ClientState::PendingDisable;
        vec![
            Action::StateChanged {
                client: id.clone(),
                state: ClientState::PendingDisable,
            },
            send(id, protocol::empty(protocol::SERVER_DISABLE_SEAT), None),
        ]
    }

    fn acknowledge_disable(&mut self, id: &ClientId) -> Vec<Action> {
        if self.clients[id].state != ClientState::PendingDisable {
            return vec![send_error(id, libc::EBUSY)];
        }
        self.clients.get_mut(id).unwrap().state = ClientState::Disabled;
        if self.active.as_ref() == Some(id) {
            self.active = None;
        }
        let mut actions = vec![Action::StateChanged {
            client: id.clone(),
            state: ClientState::Disabled,
        }];
        if self.vt_release_pending {
            if let Err(error) = self.finish_vt_release() {
                tracing::error!(%error, "could not finish VT release after disable acknowledgement");
            }
        } else {
            actions.extend(self.activate_next());
        }
        actions.push(send(
            id,
            protocol::empty(protocol::SERVER_SEAT_DISABLED),
            None,
        ));
        actions
    }

    fn activate_next(&mut self) -> Vec<Action> {
        if self.active.is_some() || self.vt_release_pending {
            return Vec::new();
        }
        let candidate = if self.vt.is_some() {
            if self.current_vt <= 0 {
                return Vec::new();
            }
            self.members.iter().find_map(|id| {
                self.clients.get(id).and_then(|client| {
                    (client.session == Some(self.current_vt)
                        && matches!(client.state, ClientState::Waiting | ClientState::Disabled))
                    .then(|| id.clone())
                })
            })
        } else {
            self.next.take().or_else(|| {
                self.members.iter().find_map(|id| {
                    self.clients.get(id).and_then(|client| {
                        matches!(client.state, ClientState::Waiting | ClientState::Disabled)
                            .then(|| id.clone())
                    })
                })
            })
        };
        candidate.map_or_else(Vec::new, |id| self.activate_client(&id))
    }

    fn activate_client(&mut self, id: &ClientId) -> Vec<Action> {
        let session = self.clients[id]
            .session
            .expect("only seat members can be activated");
        if let Some(vt) = self.vt.as_mut()
            && let Err(error) = vt.open(session)
        {
            tracing::error!(session, %error, "could not open VT for seatd client");
            return Vec::new();
        }

        let client = self.clients.get_mut(id).unwrap();
        for device in client.devices.values_mut() {
            if !device.active
                && !device.revoked
                && self.device_ops.activate(device.kind, &device.fd).is_ok()
            {
                device.active = true;
            }
        }
        client.state = ClientState::Active;
        self.active = Some(id.clone());
        vec![
            Action::StateChanged {
                client: id.clone(),
                state: ClientState::Active,
            },
            send(id, protocol::empty(protocol::SERVER_ENABLE_SEAT), None),
        ]
    }

    fn refresh_current_vt(&mut self) -> Result<(), StateError> {
        self.current_vt = self
            .vt
            .as_mut()
            .expect("VT refresh is only used for a VT-bound seat")
            .current()
            .map_err(StateError::Vt)?;
        Ok(())
    }

    fn finish_vt_release(&mut self) -> Result<(), StateError> {
        if !self.vt_release_pending {
            return Ok(());
        }
        self.vt_release_pending = false;
        let result = self
            .vt
            .as_mut()
            .expect("pending VT release requires VT operations")
            .ack_release()
            .map_err(StateError::Vt);
        self.current_vt = -1;
        result
    }

    fn revoke_authorization(&mut self, id: &DeviceId, generation: u64) -> io::Result<()> {
        let mut first_error = None;
        for (client_id, client) in &mut self.clients {
            for device in client.devices.values_mut().filter(|device| {
                device.authorization_id == *id
                    && device.generation == generation
                    && (!device.revoked || device.active)
            }) {
                device.revoked = true;
                if device.active {
                    match self.device_ops.deactivate(device.kind, &device.fd) {
                        Ok(()) => device.active = false,
                        Err(error) => {
                            tracing::error!(client = %client_id, device = %id, %error, "could not revoke removed device");
                            first_error.get_or_insert(error);
                        }
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn has_pending_revocations(&self) -> bool {
        self.clients.values().any(|client| {
            client
                .devices
                .values()
                .any(|device| device.revoked && device.active)
        })
    }

    fn lowest_free_session(&self) -> i32 {
        let used = self
            .clients
            .values()
            .filter_map(|client| client.session)
            .collect::<BTreeSet<_>>();
        (1..).find(|session| !used.contains(session)).unwrap()
    }

    fn allocate_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        generation
    }
}

impl<D: DeviceOps> Drop for SeatState<D> {
    fn drop(&mut self) {
        // Also runs on an actor panic or event-loop error. Closing only our FD
        // would leave the client's duplicate usable, particularly for evdev.
        for (client_id, client) in &mut self.clients {
            for device in client.devices.values_mut().filter(|device| device.active) {
                if let Err(error) = self.device_ops.deactivate(device.kind, &device.fd) {
                    tracing::error!(client = %client_id, device = device.protocol_id, %error,
                        "could not deactivate device during actor teardown");
                }
                device.active = false;
            }
        }
        // LinuxVtOps independently restores every owned terminal on drop.
    }
}

fn lowest_free_device_id(devices: &BTreeMap<i32, OpenDevice>) -> i32 {
    (1..)
        .find(|candidate| !devices.contains_key(candidate))
        .expect("at most 128 device IDs are allocated")
}

fn send(client: &ClientId, bytes: Vec<u8>, fd: Option<OwnedFd>) -> Action {
    Action::Send {
        client: client.clone(),
        bytes,
        fd,
    }
}

fn send_error(client: &ClientId, error: i32) -> Action {
    send(client, protocol::error(error), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeDeviceOps {
        opened: usize,
        deactivated: usize,
        activated: usize,
        fail_deactivation: bool,
    }

    impl DeviceOps for FakeDeviceOps {
        fn open(&mut self, _device: &AllowedDevice) -> io::Result<OwnedFd> {
            self.opened += 1;
            Ok(File::open("/dev/null")?.into())
        }

        fn deactivate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
            self.deactivated += 1;
            if self.fail_deactivation {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }

        fn activate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
            self.activated += 1;
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeVtState {
        current: i32,
        release_acks: usize,
        acquire_acks: usize,
        fail_release: bool,
        events: Vec<String>,
    }

    struct FakeVtOps(Arc<Mutex<FakeVtState>>);

    impl VtOps for FakeVtOps {
        fn current(&mut self) -> io::Result<i32> {
            Ok(self.0.lock().unwrap().current)
        }

        fn open(&mut self, vt: i32) -> io::Result<()> {
            self.0.lock().unwrap().events.push(format!("open:{vt}"));
            Ok(())
        }

        fn close(&mut self, vt: i32) -> io::Result<()> {
            self.0.lock().unwrap().events.push(format!("close:{vt}"));
            Ok(())
        }

        fn switch(&mut self, current: i32, target: i32) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .events
                .push(format!("switch:{current}:{target}"));
            Ok(())
        }

        fn ack_release(&mut self) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.release_acks += 1;
            state.events.push("release".into());
            if state.fail_release {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }

        fn ack_acquire(&mut self) -> io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.acquire_acks += 1;
            state.events.push("acquire".into());
            Ok(())
        }
    }

    fn vt_seat(current: i32) -> (SeatState<FakeDeviceOps>, Arc<Mutex<FakeVtState>>) {
        let vt = Arc::new(Mutex::new(FakeVtState {
            current,
            release_acks: 0,
            acquire_acks: 0,
            fail_release: false,
            events: Vec::new(),
        }));
        (
            SeatState::new_vt(
                "seat0",
                FakeDeviceOps::default(),
                Box::new(FakeVtOps(vt.clone())),
            ),
            vt,
        )
    }

    fn packet(opcode: u16, payload: &[u8]) -> Packet {
        Packet {
            opcode,
            payload: payload.to_vec(),
        }
    }

    fn path_packet(path: &str) -> Packet {
        let path = format!("{path}\0");
        let mut payload = (path.len() as u16).to_ne_bytes().to_vec();
        payload.extend_from_slice(path.as_bytes());
        packet(protocol::CLIENT_OPEN_DEVICE, &payload)
    }

    fn sent_opcodes(actions: &[Action]) -> Vec<u16> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Send { bytes, .. } => Some(u16::from_ne_bytes([bytes[0], bytes[1]])),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn invalid_seat_name_does_not_change_client_membership() {
        let mut state = SeatState::new("bad\0seat", FakeDeviceOps::default());
        let id = ClientId::new("client");
        state.add_client(id.clone()).unwrap();
        assert!(
            state
                .handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
                .is_err()
        );
        assert_eq!(state.client_state(&id), Some(ClientState::Connected));
        assert_eq!(state.client_session(&id), None);
    }

    #[test]
    fn close_then_open_reuses_one_live_protocol_connection() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();

        let opened = seat
            .handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        assert_eq!(
            sent_opcodes(&opened),
            [protocol::SERVER_SEAT_OPENED, protocol::SERVER_ENABLE_SEAT]
        );
        assert_eq!(seat.client_session(&id), Some(1));

        let closed = seat
            .handle_packet(&id, packet(protocol::CLIENT_CLOSE_SEAT, &[]))
            .unwrap();
        assert!(sent_opcodes(&closed).contains(&protocol::SERVER_SEAT_CLOSED));
        assert_eq!(seat.client_state(&id), Some(ClientState::Connected));
        assert_eq!(seat.client_session(&id), None);

        seat.handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        assert_eq!(seat.client_session(&id), Some(1));
        assert_eq!(seat.client_state(&id), Some(ClientState::Active));
    }

    #[test]
    fn non_vt_sessions_reuse_the_lowest_free_number() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let first = ClientId::new("first");
        let second = ClientId::new("second");
        let third = ClientId::new("third");
        for id in [&first, &second, &third] {
            seat.add_client(id.clone()).unwrap();
            seat.handle_packet(id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
                .unwrap();
        }
        assert_eq!(seat.client_session(&first), Some(1));
        assert_eq!(seat.client_session(&second), Some(2));
        assert_eq!(seat.client_session(&third), Some(3));
        seat.handle_packet(&second, packet(protocol::CLIENT_CLOSE_SEAT, &[]))
            .unwrap();
        seat.handle_packet(&second, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        assert_eq!(seat.client_session(&second), Some(2));
    }

    #[test]
    fn duplicate_device_open_reuses_the_record_and_reference_count() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();
        seat.handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        let metadata = fs::metadata("/dev/null").unwrap();
        seat.add_device(AllowedDevice {
            id: DeviceId::new("null"),
            path: "/dev/null".into(),
            device_number: metadata.rdev(),
            kind: DeviceKind::Evdev,
        })
        .unwrap();

        let first = seat.handle_packet(&id, path_packet("/dev/null")).unwrap();
        let second = seat.handle_packet(&id, path_packet("/dev/null")).unwrap();
        assert_eq!(sent_opcodes(&first), [protocol::SERVER_DEVICE_OPENED]);
        assert_eq!(sent_opcodes(&second), [protocol::SERVER_DEVICE_OPENED]);
        assert_eq!(seat.device_ops.opened, 1);

        for _ in 0..2 {
            seat.handle_packet(
                &id,
                packet(protocol::CLIENT_CLOSE_DEVICE, &1i32.to_ne_bytes()),
            )
            .unwrap();
        }
        assert_eq!(seat.device_ops.deactivated, 1);
    }

    #[test]
    fn authorization_is_empty_by_default_and_matches_canonical_paths_only() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();
        seat.handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        let denied = seat.handle_packet(&id, path_packet("/dev/null")).unwrap();
        assert_eq!(sent_opcodes(&denied), [protocol::SERVER_ERROR]);
        assert_eq!(seat.device_ops.opened, 0);

        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("null-alias");
        std::os::unix::fs::symlink("/dev/null", &alias).unwrap();
        let metadata = fs::metadata("/dev/null").unwrap();
        seat.add_device(
            AllowedDevice::canonicalized(
                DeviceId::new("null"),
                "/dev/null",
                metadata.rdev(),
                DeviceKind::Evdev,
            )
            .unwrap(),
        )
        .unwrap();
        let allowed = seat
            .handle_packet(&id, path_packet(alias.to_str().unwrap()))
            .unwrap();
        assert_eq!(sent_opcodes(&allowed), [protocol::SERVER_DEVICE_OPENED]);
        assert_eq!(seat.device_ops.opened, 1);

        let wrong_path = seat.handle_packet(&id, path_packet("/dev/zero")).unwrap();
        assert_eq!(sent_opcodes(&wrong_path), [protocol::SERVER_ERROR]);
        assert_eq!(seat.device_ops.opened, 1);
    }

    #[test]
    fn remove_and_readd_requires_a_new_kernel_file_description() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();
        seat.handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        let metadata = fs::metadata("/dev/null").unwrap();
        let allowed = AllowedDevice {
            id: DeviceId::new("null"),
            path: "/dev/null".into(),
            device_number: metadata.rdev(),
            kind: DeviceKind::Evdev,
        };
        seat.add_device(allowed.clone()).unwrap();
        seat.handle_packet(&id, path_packet("/dev/null")).unwrap();
        seat.remove_device(&allowed.id).unwrap();
        seat.add_device(allowed).unwrap();
        seat.handle_packet(&id, path_packet("/dev/null")).unwrap();
        assert_eq!(seat.device_ops.opened, 2);
        assert_eq!(seat.device_ops.deactivated, 1);
    }

    fn allowed(path: &str, kind: DeviceKind) -> AllowedDevice {
        AllowedDevice::canonicalized(
            DeviceId::new(path),
            path,
            fs::metadata(path).unwrap().rdev(),
            kind,
        )
        .unwrap()
    }

    fn active_seat() -> (SeatState<FakeDeviceOps>, ClientId) {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let client = ClientId::new("client");
        seat.add_client(client.clone()).unwrap();
        seat.handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        (seat, client)
    }

    #[test]
    fn dynamic_removal_revokes_all_kinds_and_preserves_duplicate_close_bookkeeping() {
        for kind in [DeviceKind::Drm, DeviceKind::Evdev, DeviceKind::Hidraw] {
            let (mut seat, client) = active_seat();
            let device = allowed("/dev/null", kind);
            seat.add_device(device.clone()).unwrap();
            seat.handle_packet(&client, path_packet("/dev/null"))
                .unwrap();
            seat.handle_packet(&client, path_packet("/dev/null"))
                .unwrap();
            seat.remove_device(&device.id).unwrap();
            seat.remove_device(&device.id).unwrap();
            assert_eq!(seat.device_ops.deactivated, 1);
            assert!(seat.clients[&client].devices[&1].revoked);
            assert!(!seat.clients[&client].devices[&1].active);
            let denied = seat
                .handle_packet(&client, path_packet("/dev/null"))
                .unwrap();
            assert_eq!(sent_opcodes(&denied), [protocol::SERVER_ERROR]);

            seat.add_device(device).unwrap();
            seat.handle_packet(&client, path_packet("/dev/null"))
                .unwrap();
            assert_eq!(seat.device_ops.opened, 2);
            assert_eq!(seat.clients[&client].devices[&2].kind, kind);
            for remaining in [1, 0] {
                let closed = seat
                    .handle_packet(
                        &client,
                        packet(protocol::CLIENT_CLOSE_DEVICE, &1i32.to_ne_bytes()),
                    )
                    .unwrap();
                assert_eq!(sent_opcodes(&closed), [protocol::SERVER_DEVICE_CLOSED]);
                assert_eq!(
                    seat.clients[&client].devices.contains_key(&1),
                    remaining != 0
                );
            }
            assert!(seat.clients[&client].devices.contains_key(&2));
            assert_eq!(seat.device_ops.deactivated, 1);
        }
    }

    #[test]
    fn replacements_preserve_unchanged_opens_and_revoke_changed_identity() {
        let (mut seat, client) = active_seat();
        let original = allowed("/dev/null", DeviceKind::Evdev);
        seat.replace_devices(vec![original.clone()]).unwrap();
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        seat.replace_devices(vec![original.clone()]).unwrap();
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        assert_eq!(seat.device_ops.opened, 1);
        assert_eq!(seat.device_ops.deactivated, 0);

        let mut changed = original;
        changed.kind = DeviceKind::Drm;
        seat.replace_devices(vec![changed]).unwrap();
        assert_eq!(seat.device_ops.deactivated, 1);
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        assert_eq!(seat.device_ops.opened, 2);
        assert_eq!(seat.clients[&client].devices[&2].kind, DeviceKind::Drm);
    }

    #[test]
    fn invalid_device_commands_leave_authorization_and_opens_unchanged() {
        let (mut seat, client) = active_seat();
        let device = allowed("/dev/null", DeviceKind::Evdev);
        seat.add_device(device.clone()).unwrap();
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        assert!(
            seat.replace_devices(vec![device.clone(), device.clone()])
                .is_err()
        );
        let mut alias = device.clone();
        alias.id = DeviceId::new("alias");
        assert!(
            seat.replace_devices(vec![device.clone(), alias.clone()])
                .is_err()
        );
        assert!(seat.add_device(alias).is_err());
        assert_eq!(seat.allowed.len(), 1);
        assert_eq!(seat.device_ops.deactivated, 0);
        assert!(!seat.clients[&client].devices[&1].revoked);
    }

    #[test]
    fn failed_removal_denies_opens_blocks_readd_and_retries_the_ioctl() {
        let (mut seat, client) = active_seat();
        let device = allowed("/dev/null", DeviceKind::Evdev);
        seat.add_device(device.clone()).unwrap();
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        seat.device_ops.fail_deactivation = true;
        assert!(seat.remove_device(&device.id).is_err());
        assert!(seat.allowed.is_empty());
        assert!(seat.clients[&client].devices[&1].revoked);
        assert!(seat.clients[&client].devices[&1].active);
        let denied = seat
            .handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        assert_eq!(sent_opcodes(&denied), [protocol::SERVER_ERROR]);
        assert!(seat.add_device(device.clone()).is_err());
        assert!(seat.replace_devices(vec![device.clone()]).is_err());
        assert_eq!(seat.device_ops.deactivated, 2);
        assert!(seat.allowed.is_empty());

        seat.device_ops.fail_deactivation = false;
        seat.remove_device(&device.id).unwrap();
        assert_eq!(seat.device_ops.deactivated, 3);
        seat.add_device(device).unwrap();
        seat.handle_packet(&client, path_packet("/dev/null"))
            .unwrap();
        assert_eq!(seat.device_ops.opened, 2);
    }

    #[test]
    fn replacement_attempts_all_revocations_before_reporting_failure() {
        let (mut seat, client) = active_seat();
        let first = allowed("/dev/null", DeviceKind::Evdev);
        let second = allowed("/dev/zero", DeviceKind::Evdev);
        seat.replace_devices(vec![first.clone(), second.clone()])
            .unwrap();
        for path in ["/dev/null", "/dev/zero"] {
            seat.handle_packet(&client, path_packet(path)).unwrap();
        }
        seat.device_ops.fail_deactivation = true;
        assert!(seat.replace_devices(Vec::new()).is_err());
        assert_eq!(seat.device_ops.deactivated, 2);
        assert!(seat.allowed.is_empty());
        seat.device_ops.fail_deactivation = false;
        seat.replace_devices(vec![first, second]).unwrap();
        assert_eq!(seat.device_ops.deactivated, 4);
        assert_eq!(seat.allowed.len(), 2);
    }

    #[test]
    fn removed_drm_records_are_never_reactivated_after_a_session_switch() {
        let (mut seat, first) = active_seat();
        let second = ClientId::new("second");
        seat.add_client(second.clone()).unwrap();
        seat.handle_packet(&second, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        let device = allowed("/dev/null", DeviceKind::Drm);
        seat.add_device(device.clone()).unwrap();
        seat.handle_packet(&first, path_packet("/dev/null"))
            .unwrap();
        seat.remove_device(&device.id).unwrap();
        seat.add_device(device).unwrap();
        for (source, target) in [(&first, 2i32), (&second, 1i32)] {
            seat.handle_packet(
                source,
                packet(protocol::CLIENT_SWITCH_SESSION, &target.to_ne_bytes()),
            )
            .unwrap();
            seat.handle_packet(source, packet(protocol::CLIENT_DISABLE_SEAT, &[]))
                .unwrap();
        }
        assert_eq!(seat.client_state(&first), Some(ClientState::Active));
        assert_eq!(seat.device_ops.activated, 0);
        assert!(seat.clients[&first].devices[&1].revoked);
    }

    #[test]
    fn switch_waits_for_disable_ack_before_enabling_the_target() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let first = ClientId::new("first");
        let second = ClientId::new("second");
        for id in [&first, &second] {
            seat.add_client(id.clone()).unwrap();
            seat.handle_packet(id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
                .unwrap();
        }
        let switching = seat
            .handle_packet(
                &first,
                packet(protocol::CLIENT_SWITCH_SESSION, &2i32.to_ne_bytes()),
            )
            .unwrap();
        assert!(sent_opcodes(&switching).contains(&protocol::SERVER_DISABLE_SEAT));
        assert_eq!(seat.client_state(&first), Some(ClientState::PendingDisable));
        assert_eq!(seat.client_state(&second), Some(ClientState::Waiting));

        let acknowledged = seat
            .handle_packet(&first, packet(protocol::CLIENT_DISABLE_SEAT, &[]))
            .unwrap();
        assert!(sent_opcodes(&acknowledged).contains(&protocol::SERVER_ENABLE_SEAT));
        assert_eq!(seat.client_state(&first), Some(ClientState::Disabled));
        assert_eq!(seat.client_state(&second), Some(ClientState::Active));
    }

    #[test]
    fn removal_completes_when_vt_acknowledgement_fails() {
        let (mut seat, vt) = vt_seat(7);
        let client = ClientId::new("client");
        seat.add_client(client.clone()).unwrap();
        seat.handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        seat.handle_vt_release().unwrap();
        vt.lock().unwrap().fail_release = true;
        let actions = seat.revoke_client(&client).unwrap();
        assert!(actions.iter().any(|action| matches!(action,
            Action::ClientGone(id) if id == &client)));
        assert_eq!(seat.client_state(&client), None);
        assert!(!seat.vt_release_pending());
        assert_eq!(vt.lock().unwrap().release_acks, 1);
        assert!(seat.handle_eof(&client).unwrap().is_empty());
    }

    #[test]
    fn vt_release_without_an_active_client_is_immediate() {
        let (mut seat, vt) = vt_seat(7);
        assert!(seat.handle_vt_release().unwrap().is_empty());
        assert!(!seat.vt_release_pending());
        assert_eq!(vt.lock().unwrap().release_acks, 1);
    }

    #[test]
    fn vt_release_waits_for_disable_ack_and_late_ack_is_idempotent() {
        let (mut seat, vt) = vt_seat(7);
        let client = ClientId::new("greeter");
        seat.add_client(client.clone()).unwrap();
        seat.handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();

        let release = seat.handle_vt_release().unwrap();
        assert_eq!(
            seat.client_state(&client),
            Some(ClientState::PendingDisable)
        );
        assert!(sent_opcodes(&release).contains(&protocol::SERVER_DISABLE_SEAT));
        assert_eq!(vt.lock().unwrap().release_acks, 0);

        let ack = seat
            .handle_packet(&client, packet(protocol::CLIENT_DISABLE_SEAT, &[]))
            .unwrap();
        assert_eq!(seat.client_state(&client), Some(ClientState::Disabled));
        assert_eq!(vt.lock().unwrap().release_acks, 1);
        assert!(sent_opcodes(&ack).contains(&protocol::SERVER_SEAT_DISABLED));

        let late = seat
            .handle_packet(&client, packet(protocol::CLIENT_DISABLE_SEAT, &[]))
            .unwrap();
        assert_eq!(sent_opcodes(&late), [protocol::SERVER_ERROR]);
        assert_eq!(vt.lock().unwrap().release_acks, 1);
    }

    #[test]
    fn disconnect_and_timeout_complete_a_pending_vt_release() {
        let (mut disconnected, disconnected_vt) = vt_seat(7);
        let client = ClientId::new("disconnect");
        disconnected.add_client(client.clone()).unwrap();
        disconnected
            .handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        disconnected.handle_vt_release().unwrap();
        disconnected.handle_eof(&client).unwrap();
        let state = disconnected_vt.lock().unwrap();
        assert_eq!(state.release_acks, 1);
        assert!(state.events.contains(&"close:7".to_owned()));
        drop(state);

        let (mut timed_out, timeout_vt) = vt_seat(8);
        let client = ClientId::new("timeout");
        timed_out.add_client(client.clone()).unwrap();
        timed_out
            .handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        timed_out.handle_vt_release().unwrap();
        let actions = timed_out.handle_vt_release_timeout().unwrap();
        assert_eq!(timed_out.client_state(&client), Some(ClientState::Disabled));
        assert_eq!(timeout_vt.lock().unwrap().release_acks, 1);
        assert!(actions.iter().any(|action| matches!(
            action,
            Action::StateChanged {
                client: changed,
                state: ClientState::Disabled
            } if changed == &client
        )));
    }

    #[test]
    fn tty7_to_tty8_handoff_releases_before_acquiring_and_enabling() {
        let (mut seat, vt) = vt_seat(7);
        let greeter = ClientId::new("greeter");
        seat.add_client(greeter.clone()).unwrap();
        seat.handle_packet(&greeter, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        seat.handle_vt_release().unwrap();
        seat.handle_packet(&greeter, packet(protocol::CLIENT_DISABLE_SEAT, &[]))
            .unwrap();

        let session = ClientId::new("session");
        seat.add_client(session.clone()).unwrap();
        seat.clients.get_mut(&session).unwrap().session = Some(8);
        seat.clients.get_mut(&session).unwrap().state = ClientState::Waiting;
        seat.members.push_back(session.clone());
        vt.lock().unwrap().current = 8;

        let acquire = seat.handle_vt_acquire().unwrap();
        assert_eq!(seat.client_state(&session), Some(ClientState::Active));
        assert!(sent_opcodes(&acquire).contains(&protocol::SERVER_ENABLE_SEAT));
        let state = vt.lock().unwrap();
        assert_eq!(state.release_acks, 1);
        assert_eq!(state.acquire_acks, 1);
        let release = state
            .events
            .iter()
            .position(|event| event == "release")
            .unwrap();
        let acquire = state
            .events
            .iter()
            .position(|event| event == "acquire")
            .unwrap();
        let open = state
            .events
            .iter()
            .position(|event| event == "open:8")
            .unwrap();
        assert!(release < acquire && acquire < open);
    }

    #[test]
    fn vt_switch_refreshes_the_foreground_vt_and_waits_for_kernel_signals() {
        let (mut seat, vt) = vt_seat(7);
        let client = ClientId::new("client");
        seat.add_client(client.clone()).unwrap();
        seat.handle_packet(&client, packet(protocol::CLIENT_OPEN_SEAT, &[]))
            .unwrap();
        vt.lock().unwrap().current = 9;
        let switched = seat
            .handle_packet(
                &client,
                packet(protocol::CLIENT_SWITCH_SESSION, &10i32.to_ne_bytes()),
            )
            .unwrap();
        assert_eq!(sent_opcodes(&switched), [protocol::SERVER_SESSION_SWITCHED]);
        assert_eq!(seat.client_state(&client), Some(ClientState::Active));
        assert!(
            vt.lock()
                .unwrap()
                .events
                .contains(&"switch:9:10".to_owned())
        );
    }

    #[test]
    fn seat_scoped_requests_without_membership_are_protocol_errors() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();
        for request in [
            path_packet("/dev/null"),
            packet(protocol::CLIENT_CLOSE_DEVICE, &1i32.to_ne_bytes()),
            packet(protocol::CLIENT_DISABLE_SEAT, &[]),
            packet(protocol::CLIENT_SWITCH_SESSION, &1i32.to_ne_bytes()),
        ] {
            assert!(matches!(
                seat.handle_packet(&id, request),
                Err(StateError::Protocol(client)) if client == id
            ));
        }
    }

    #[test]
    fn an_already_queued_non_vt_switch_is_reported_as_success() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let first = ClientId::new("first");
        let second = ClientId::new("second");
        for id in [&first, &second] {
            seat.add_client(id.clone()).unwrap();
            seat.handle_packet(id, packet(protocol::CLIENT_OPEN_SEAT, &[]))
                .unwrap();
        }
        seat.next = Some(second);
        let queued = seat.switch_session(&first, 99);
        assert_eq!(sent_opcodes(&queued), [protocol::SERVER_SESSION_SWITCHED]);
    }

    #[test]
    fn malformed_messages_are_protocol_errors() {
        let mut seat = SeatState::new("seat-test", FakeDeviceOps::default());
        let id = ClientId::new("client");
        seat.add_client(id.clone()).unwrap();
        assert!(matches!(
            seat.handle_packet(&id, packet(protocol::CLIENT_OPEN_SEAT, &[0])),
            Err(StateError::Protocol(client)) if client == id
        ));
    }
}
