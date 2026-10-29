use crate::connection::{Connection, ReadStatus};
use crate::device::{AllowedDevice, DeviceId, DeviceOps, KernelDeviceOps};
use crate::state::{Action, ClientId, ClientState, SeatState, StateError};
use crate::vt::{LinuxVtOps, VtOps};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CLIENT_PACKET_BATCH: usize = 32;
const VT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeatConfig {
    pub name: String,
    pub vt_bound: bool,
}

#[derive(Debug)]
pub enum SeatEvent {
    ClientGone {
        seat: String,
        client: ClientId,
    },
    ClientStateChanged {
        seat: String,
        client: ClientId,
        state: ClientState,
    },
    ActorFailed {
        seat: String,
        message: String,
    },
}

struct EventQueue {
    queue: Mutex<VecDeque<SeatEvent>>,
    wake: OwnedFd,
}

impl EventQueue {
    fn new() -> io::Result<Self> {
        Ok(Self {
            queue: Mutex::new(VecDeque::new()),
            wake: new_eventfd()?,
        })
    }

    fn push(&self, event: SeatEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(event);
            let _ = wake_eventfd(&self.wake);
        }
    }

    fn drain(&self) -> Vec<SeatEvent> {
        self.queue
            .lock()
            .map(|mut queue| {
                let _ = drain_eventfd(&self.wake);
                queue.drain(..).collect()
            })
            .unwrap_or_default()
    }
}

pub struct ServerRuntime {
    events: Arc<EventQueue>,
}

impl ServerRuntime {
    pub fn initialize() -> io::Result<Self> {
        crate::vt::block_vt_signals()?;
        Ok(Self {
            events: Arc::new(EventQueue::new()?),
        })
    }

    pub fn event_fd(&self) -> BorrowedFd<'_> {
        self.events.wake.as_fd()
    }

    pub fn drain_events(&self) -> Vec<SeatEvent> {
        self.events.drain()
    }

    pub fn spawn_seat(&self, config: SeatConfig) -> io::Result<SeatHandle> {
        let vt_ops = config
            .vt_bound
            .then(|| Box::<LinuxVtOps>::default() as Box<dyn VtOps>);
        self.spawn_with_ops(config, Box::<KernelDeviceOps>::default(), vt_ops)
    }

    #[cfg(test)]
    fn spawn_with_device_ops(
        &self,
        config: SeatConfig,
        device_ops: Box<dyn DeviceOps>,
    ) -> io::Result<SeatHandle> {
        let vt_ops = config
            .vt_bound
            .then(|| Box::<LinuxVtOps>::default() as Box<dyn VtOps>);
        self.spawn_with_ops(config, device_ops, vt_ops)
    }

    fn spawn_with_ops(
        &self,
        config: SeatConfig,
        device_ops: Box<dyn DeviceOps>,
        vt_ops: Option<Box<dyn VtOps>>,
    ) -> io::Result<SeatHandle> {
        // Reject invalid names before spawning a worker or publishing a seat.
        crate::protocol::seat_opened(&config.name)?;
        let mailbox = Arc::new(CommandMailbox {
            queue: Mutex::new(CommandQueue::default()),
            wake: new_eventfd()?,
        });
        let actor_mailbox = mailbox.clone();
        let mailbox_for_exit = mailbox.clone();
        let events = self.events.clone();
        let seat_name = config.name.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let thread = thread::Builder::new()
            .name(format!("seatd-{seat_name}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    match Actor::new(
                        seat_name.clone(),
                        actor_mailbox,
                        events.clone(),
                        device_ops,
                        vt_ops,
                    ) {
                        Ok(actor) => {
                            let _ = startup_sender.send(Ok(()));
                            actor.run()
                        }
                        Err(error) => {
                            let _ = startup_sender.send(Err(copy_io_error(&error)));
                            Err(error)
                        }
                    }
                }));
                // Atomically reject future requests and drop queued replies. Without
                // this, a request racing an actor exit can wait forever because
                // its reply sender remains owned by the handle's mailbox.
                if let Ok(mut queue) = mailbox_for_exit.queue.lock() {
                    queue.closed = true;
                    queue.commands.clear();
                }
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => events.push(SeatEvent::ActorFailed {
                        seat: seat_name,
                        message: error.to_string(),
                    }),
                    Err(payload) => events.push(SeatEvent::ActorFailed {
                        seat: seat_name,
                        message: panic_message(payload),
                    }),
                }
            })?;
        match startup_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(io::Error::other("seat actor failed during startup"));
            }
        }
        Ok(SeatHandle {
            name: config.name,
            mailbox,
            thread: Some(thread),
            stopped: false,
        })
    }
}

#[derive(Default)]
struct CommandQueue {
    commands: VecDeque<Command>,
    closed: bool,
}

struct CommandMailbox {
    queue: Mutex<CommandQueue>,
    wake: OwnedFd,
}

type Reply = SyncSender<io::Result<()>>;

enum Command {
    ReplaceDevices(Vec<AllowedDevice>, Reply),
    AddDevice(AllowedDevice, Reply),
    RemoveDevice(DeviceId, Reply),
    AddClient(ClientId, OwnedFd, Reply),
    RevokeClient(ClientId, Reply),
    Shutdown(Reply),
}

pub struct SeatHandle {
    name: String,
    mailbox: Arc<CommandMailbox>,
    thread: Option<JoinHandle<()>>,
    stopped: bool,
}

impl SeatHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn replace_devices(&self, devices: Vec<AllowedDevice>) -> io::Result<()> {
        self.request(|reply| Command::ReplaceDevices(devices, reply))
    }

    pub fn add_device(&self, device: AllowedDevice) -> io::Result<()> {
        self.request(|reply| Command::AddDevice(device, reply))
    }

    pub fn remove_device(&self, id: DeviceId) -> io::Result<()> {
        self.request(|reply| Command::RemoveDevice(id, reply))
    }

    pub fn add_client(&self, id: ClientId, connection: OwnedFd) -> io::Result<()> {
        self.request(|reply| Command::AddClient(id, connection, reply))
    }

    pub fn revoke_client(&self, id: ClientId) -> io::Result<()> {
        self.request(|reply| Command::RevokeClient(id, reply))
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        let result = self.request(Command::Shutdown);
        self.stopped = true;
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                return Err(io::Error::other("seat actor panicked"));
            }
        }
        result
    }

    fn request(&self, command: impl FnOnce(Reply) -> Command) -> io::Result<()> {
        let (sender, receiver) = sync_channel(1);
        {
            let mut queue = self
                .mailbox
                .queue
                .lock()
                .map_err(|_| io::Error::other("seat actor command queue is poisoned"))?;
            if queue.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "seat actor stopped",
                ));
            }
            queue.commands.push_back(command(sender));
            wake_eventfd(&self.mailbox.wake)?;
        }
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "seat actor stopped"))?
    }
}

impl Drop for SeatHandle {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let (sender, _receiver) = sync_channel(1);
        if let Ok(mut queue) = self.mailbox.queue.lock() {
            if !queue.closed {
                queue.commands.push_back(Command::Shutdown(sender));
                let _ = wake_eventfd(&self.mailbox.wake);
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.stopped = true;
    }
}

struct Actor {
    name: String,
    mailbox: Arc<CommandMailbox>,
    events: Arc<EventQueue>,
    state: SeatState<Box<dyn DeviceOps>>,
    connections: BTreeMap<ClientId, Connection>,
    pending: VecDeque<ClientId>,
    pending_set: BTreeSet<ClientId>,
    vt_signal: Option<OwnedFd>,
    vt_timer: Option<OwnedFd>,
    vt_timer_armed: bool,
}

impl Actor {
    fn new(
        name: String,
        mailbox: Arc<CommandMailbox>,
        events: Arc<EventQueue>,
        device_ops: Box<dyn DeviceOps>,
        vt_ops: Option<Box<dyn VtOps>>,
    ) -> io::Result<Self> {
        let vt_bound = vt_ops.is_some();
        let state = match vt_ops {
            Some(vt_ops) => SeatState::new_vt(name.clone(), device_ops, vt_ops),
            None => SeatState::new(name.clone(), device_ops),
        };
        Ok(Self {
            name,
            mailbox,
            events,
            state,
            connections: BTreeMap::new(),
            pending: VecDeque::new(),
            pending_set: BTreeSet::new(),
            vt_signal: vt_bound.then(crate::vt::signalfd).transpose()?,
            vt_timer: vt_bound.then(new_timerfd).transpose()?,
            vt_timer_armed: false,
        })
    }

    fn run(mut self) -> io::Result<()> {
        loop {
            if let Some(shutdown) = self.process_commands()? {
                return shutdown;
            }
            if let Some(id) = self.pop_pending() {
                self.process_client_packets(&id)?;
                if let Some(shutdown) = self.process_commands()? {
                    return shutdown;
                }
            }

            self.sync_vt_timer()?;
            let ids = self.connections.keys().cloned().collect::<Vec<_>>();
            let mut poll_fds = Vec::with_capacity(ids.len() + 3);
            poll_fds.push(PollFd::new(
                self.mailbox.wake.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
            ));
            let signal_index = self.vt_signal.as_ref().map(|fd| {
                let index = poll_fds.len();
                poll_fds.push(PollFd::new(
                    fd.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
                ));
                index
            });
            let timer_index = self.vt_timer.as_ref().map(|fd| {
                let index = poll_fds.len();
                poll_fds.push(PollFd::new(
                    fd.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP,
                ));
                index
            });
            let client_start = poll_fds.len();
            for id in &ids {
                let connection = &self.connections[id];
                let mut events = PollFlags::POLLERR | PollFlags::POLLHUP;
                // Drain already-buffered frames before reading more: a full
                // recvmsg could otherwise overflow the bounded decoder.
                if !self.pending_set.contains(id) && connection.can_process_input() {
                    events |= PollFlags::POLLIN;
                }
                if connection.wants_write() {
                    events |= PollFlags::POLLOUT;
                }
                poll_fds.push(PollFd::new(connection.fd(), events));
            }
            let timeout = if self.pending.is_empty() {
                PollTimeout::NONE
            } else {
                PollTimeout::ZERO
            };
            match poll(&mut poll_fds, timeout) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(errno_to_io(error)),
            }
            let ready = poll_fds
                .iter()
                .map(|descriptor| descriptor.revents().unwrap_or_else(PollFlags::empty))
                .collect::<Vec<_>>();
            drop(poll_fds);

            if ready[0].intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP) {
                continue;
            }
            if signal_index.is_some_and(|index| {
                ready[index].intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP)
            }) {
                self.process_vt_signals()?;
                self.sync_vt_timer()?;
            }
            if timer_index.is_some_and(|index| {
                ready[index].intersects(PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP)
            }) {
                self.process_vt_timeout()?;
                self.sync_vt_timer()?;
            }
            for (id, events) in ids.into_iter().zip(ready.into_iter().skip(client_start)) {
                if let Some(shutdown) = self.process_commands()? {
                    return shutdown;
                }
                if !self.connections.contains_key(&id) {
                    continue;
                }
                if events.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL) {
                    self.disconnect(&id);
                    continue;
                }
                if events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP)
                    && !self.pending_set.contains(&id)
                {
                    match self.connections.get_mut(&id).unwrap().read() {
                        Ok(ReadStatus::Progress | ReadStatus::Eof | ReadStatus::WouldBlock) => {}
                        Err(error) => {
                            tracing::warn!(seat = %self.name, client = %id, %error, "seatd client read failed");
                            self.disconnect(&id);
                            continue;
                        }
                    }
                    if self.connections[&id].received_fd_count() != 0 {
                        tracing::warn!(seat = %self.name, client = %id, "seatd client sent an unexpected descriptor");
                        self.disconnect(&id);
                        continue;
                    }
                    self.process_client_packets(&id)?;
                }
                if self.connections.contains_key(&id) && events.contains(PollFlags::POLLOUT) {
                    if self.connections.get_mut(&id).unwrap().flush().is_err() {
                        self.disconnect(&id);
                    } else if self.connections.contains_key(&id)
                        && self.connections[&id].can_process_input()
                        && self.connections[&id].has_complete_packet()?
                    {
                        self.push_pending(id);
                    }
                }
            }
        }
    }

    fn process_vt_signals(&mut self) -> io::Result<()> {
        let Some(signal_fd) = self.vt_signal.as_ref() else {
            return Ok(());
        };
        let mut signals = Vec::new();
        while let Some(signal) = crate::vt::read_signal(signal_fd)? {
            signals.push(signal);
        }
        for signal in signals {
            let actions = match signal {
                libc::SIGUSR1 => self.state.handle_vt_release(),
                libc::SIGUSR2 => self.state.handle_vt_acquire(),
                _ => continue,
            }
            .map_err(state_to_io)?;
            self.apply(actions);
        }
        Ok(())
    }

    fn process_vt_timeout(&mut self) -> io::Result<()> {
        let Some(timer) = self.vt_timer.as_ref() else {
            return Ok(());
        };
        drain_timerfd(timer)?;
        self.vt_timer_armed = false;
        let actions = self
            .state
            .handle_vt_release_timeout()
            .map_err(state_to_io)?;
        self.apply(actions);
        Ok(())
    }

    fn sync_vt_timer(&mut self) -> io::Result<()> {
        let Some(timer) = self.vt_timer.as_ref() else {
            return Ok(());
        };
        match (self.state.vt_release_pending(), self.vt_timer_armed) {
            (true, false) => {
                set_timerfd(timer, Some(VT_RELEASE_TIMEOUT))?;
                self.vt_timer_armed = true;
            }
            (false, true) => {
                set_timerfd(timer, None)?;
                self.vt_timer_armed = false;
            }
            _ => {}
        }
        Ok(())
    }

    fn process_commands(&mut self) -> io::Result<Option<io::Result<()>>> {
        let commands = {
            let mut queue = self
                .mailbox
                .queue
                .lock()
                .map_err(|_| io::Error::other("seat actor command queue is poisoned"))?;
            if queue.commands.is_empty() {
                return Ok(None);
            }
            let _ = drain_eventfd(&self.mailbox.wake);
            queue.commands.drain(..).collect::<Vec<_>>()
        };

        for command in commands {
            match command {
                Command::ReplaceDevices(devices, reply) => {
                    let result = self
                        .state
                        .replace_devices(devices)
                        .map(|actions| self.apply(actions))
                        .map_err(state_to_io);
                    let _ = reply.send(result);
                }
                Command::AddDevice(device, reply) => {
                    let result = self
                        .state
                        .add_device(device)
                        .map(|actions| self.apply(actions))
                        .map_err(state_to_io);
                    let _ = reply.send(result);
                }
                Command::RemoveDevice(id, reply) => {
                    let result = self
                        .state
                        .remove_device(&id)
                        .map(|actions| self.apply(actions))
                        .map_err(state_to_io);
                    let _ = reply.send(result);
                }
                Command::AddClient(id, fd, reply) => {
                    let result = Connection::new(fd).and_then(|connection| {
                        self.state.add_client(id.clone()).map_err(state_to_io)?;
                        self.connections.insert(id, connection);
                        Ok(())
                    });
                    let _ = reply.send(result);
                }
                Command::RevokeClient(id, reply) => {
                    let result = self
                        .state
                        .revoke_client(&id)
                        .map(|actions| {
                            self.connections.remove(&id);
                            self.remove_pending(&id);
                            self.apply(actions);
                        })
                        .map_err(state_to_io);
                    let _ = reply.send(result);
                }
                Command::Shutdown(reply) => {
                    for id in self.state.client_ids() {
                        self.connections.remove(&id);
                        if let Ok(actions) = self.state.handle_eof(&id) {
                            self.apply(actions);
                        }
                    }
                    let result = Ok(());
                    let _ = reply.send(Ok(()));
                    return Ok(Some(result));
                }
            }
        }
        Ok(None)
    }

    fn process_client_packets(&mut self, id: &ClientId) -> io::Result<()> {
        for _ in 0..CLIENT_PACKET_BATCH {
            let packet = match self.connections.get_mut(id) {
                Some(connection) => connection.next_packet(),
                None => return Ok(()),
            };
            let packet = match packet {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(seat = %self.name, client = %id, %error, "invalid seatd frame");
                    self.disconnect(id);
                    return Ok(());
                }
            };
            match self.state.handle_packet(id, packet) {
                Ok(actions) => self.apply(actions),
                Err(StateError::Protocol(_)) => {
                    tracing::warn!(seat = %self.name, client = %id, "seatd protocol violation");
                    self.disconnect(id);
                    return Ok(());
                }
                Err(error) => return Err(state_to_io(error)),
            }
        }

        let Some(connection) = self.connections.get(id) else {
            return Ok(());
        };
        match connection.has_complete_packet() {
            Ok(true) if connection.can_process_input() => self.push_pending(id.clone()),
            Ok(true) => {}
            Ok(false) if connection.is_eof() && connection.pending_input() == 0 => {
                self.disconnect(id)
            }
            Ok(false) if connection.is_eof() => {
                tracing::warn!(seat = %self.name, client = %id, "truncated seatd packet at EOF");
                self.disconnect(id);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(seat = %self.name, client = %id, %error, "invalid seatd frame");
                self.disconnect(id);
            }
        }
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>) {
        let mut failed = BTreeSet::new();
        for action in actions {
            match action {
                Action::Send { client, bytes, fd } => {
                    if let Some(connection) = self.connections.get_mut(&client) {
                        if let Err(error) = connection.queue(bytes, fd) {
                            tracing::warn!(seat = %self.name, %client, %error, "disconnecting stalled seatd client");
                            failed.insert(client);
                        }
                    }
                }
                Action::StateChanged { client, state } => {
                    self.events.push(SeatEvent::ClientStateChanged {
                        seat: self.name.clone(),
                        client,
                        state,
                    });
                }
                Action::ClientGone(client) => {
                    self.events.push(SeatEvent::ClientGone {
                        seat: self.name.clone(),
                        client,
                    });
                }
            }
        }
        for client in failed {
            self.disconnect(&client);
        }
    }

    fn disconnect(&mut self, id: &ClientId) {
        if self.connections.remove(id).is_none() {
            return;
        }
        self.remove_pending(id);
        if let Ok(actions) = self.state.handle_eof(id) {
            self.apply(actions);
        }
    }

    fn push_pending(&mut self, id: ClientId) {
        if self.pending_set.insert(id.clone()) {
            self.pending.push_back(id);
        }
    }

    fn pop_pending(&mut self) -> Option<ClientId> {
        let id = self.pending.pop_front()?;
        self.pending_set.remove(&id);
        Some(id)
    }

    fn remove_pending(&mut self, id: &ClientId) {
        if self.pending_set.remove(id) {
            self.pending.retain(|candidate| candidate != id);
        }
    }
}

fn new_timerfd() -> io::Result<OwnedFd> {
    // SAFETY: timerfd_create returns a new descriptor owned by the caller.
    let fd = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the new descriptor is transferred exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn set_timerfd(fd: &OwnedFd, timeout: Option<Duration>) -> io::Result<()> {
    let duration = timeout.unwrap_or(Duration::ZERO);
    let value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: duration.as_secs().try_into().unwrap_or(libc::time_t::MAX),
            tv_nsec: duration.subsec_nanos().into(),
        },
    };
    // SAFETY: value is fully initialized and fd is a timerfd owned by the actor.
    if unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &value, std::ptr::null_mut()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn drain_timerfd(fd: &OwnedFd) -> io::Result<()> {
    let mut expirations = 0u64;
    loop {
        // SAFETY: expirations points to eight writable bytes and fd is a timerfd.
        let result = unsafe {
            libc::read(
                fd.as_raw_fd(),
                (&mut expirations as *mut u64).cast(),
                size_of::<u64>(),
            )
        };
        if result == size_of::<u64>() as isize {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(()),
            _ => return Err(error),
        }
    }
}

fn new_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: eventfd returns a new descriptor owned by the caller.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the new descriptor is transferred exactly once.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn wake_eventfd(fd: &OwnedFd) -> io::Result<()> {
    let value = 1u64.to_ne_bytes();
    loop {
        // SAFETY: value points to eight readable bytes and fd is an eventfd.
        let result = unsafe { libc::write(fd.as_raw_fd(), value.as_ptr().cast(), value.len()) };
        if result == value.len() as isize {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(()),
            _ => return Err(error),
        }
    }
}

fn drain_eventfd(fd: &OwnedFd) -> io::Result<()> {
    let mut value = [0u8; 8];
    loop {
        // SAFETY: value points to eight writable bytes and fd is an eventfd.
        let result = unsafe { libc::read(fd.as_raw_fd(), value.as_mut_ptr().cast(), value.len()) };
        if result == value.len() as isize {
            continue;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(()),
            _ => return Err(error),
        }
    }
}

fn state_to_io(error: StateError) -> io::Error {
    match error {
        StateError::Device(error) | StateError::Vt(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidInput, error),
    }
}

fn errno_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn copy_io_error(error: &io::Error) -> io::Error {
    error.raw_os_error().map_or_else(
        || io::Error::new(error.kind(), error.to_string()),
        io::Error::from_raw_os_error,
    )
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "seat actor panicked".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceKind, DeviceOps};
    use crate::protocol;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::io::{Read, Write};
    use std::os::fd::OwnedFd;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct FakeDeviceOps;

    impl DeviceOps for FakeDeviceOps {
        fn open(&mut self, _device: &AllowedDevice) -> io::Result<OwnedFd> {
            Ok(std::fs::File::open("/dev/null")?.into())
        }

        fn deactivate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
            Ok(())
        }

        fn activate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeVtOps {
        current: i32,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl VtOps for FakeVtOps {
        fn current(&mut self) -> io::Result<i32> {
            Ok(self.current)
        }

        fn open(&mut self, vt: i32) -> io::Result<()> {
            self.events.lock().unwrap().push(format!("open:{vt}"));
            Ok(())
        }

        fn close(&mut self, vt: i32) -> io::Result<()> {
            self.events.lock().unwrap().push(format!("close:{vt}"));
            Ok(())
        }

        fn switch(&mut self, _current: i32, _target: i32) -> io::Result<()> {
            Ok(())
        }

        fn ack_release(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn ack_acquire(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn actor() -> (ServerRuntime, SeatHandle) {
        let runtime = ServerRuntime::initialize().unwrap();
        let handle = runtime
            .spawn_with_device_ops(
                SeatConfig {
                    name: "seat-test".into(),
                    vt_bound: false,
                },
                Box::<FakeDeviceOps>::default(),
            )
            .unwrap();
        (runtime, handle)
    }

    fn client(handle: &SeatHandle, id: &str) -> std::os::unix::net::UnixStream {
        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        handle.add_client(ClientId::new(id), server).unwrap();
        client.into()
    }

    fn read_packet(stream: &mut std::os::unix::net::UnixStream) -> (u16, Vec<u8>) {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).unwrap();
        let opcode = u16::from_ne_bytes([header[0], header[1]]);
        let size = u16::from_ne_bytes([header[2], header[3]]) as usize;
        let mut payload = vec![0; size];
        stream.read_exact(&mut payload).unwrap();
        (opcode, payload)
    }

    #[test]
    fn actor_panic_deactivates_open_devices_and_reports_a_critical_failure() {
        use std::os::unix::fs::MetadataExt;
        struct PanicOps(Arc<Mutex<usize>>);
        impl DeviceOps for PanicOps {
            fn open(&mut self, device: &AllowedDevice) -> io::Result<OwnedFd> {
                assert_ne!(
                    device.path,
                    std::path::Path::new("/dev/zero"),
                    "injected driver panic"
                );
                Ok(std::fs::File::open("/dev/null")?.into())
            }
            fn deactivate(&mut self, _: DeviceKind, _: &OwnedFd) -> io::Result<()> {
                *self.0.lock().unwrap() += 1;
                Ok(())
            }
            fn activate(&mut self, _: DeviceKind, _: &OwnedFd) -> io::Result<()> {
                Ok(())
            }
        }
        let runtime = ServerRuntime::initialize().unwrap();
        let revoked = Arc::new(Mutex::new(0));
        let handle = runtime
            .spawn_with_device_ops(
                SeatConfig {
                    name: "seat-test".into(),
                    vt_bound: false,
                },
                Box::new(PanicOps(revoked.clone())),
            )
            .unwrap();
        let devices = ["/dev/null", "/dev/zero"]
            .into_iter()
            .map(|path| {
                AllowedDevice::canonicalized(
                    DeviceId::new(path),
                    path,
                    std::fs::metadata(path).unwrap().rdev(),
                    DeviceKind::Evdev,
                )
                .unwrap()
            })
            .collect();
        handle.replace_devices(devices).unwrap();
        let mut stream = client(&handle, "client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_SEAT_OPENED);
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_ENABLE_SEAT);
        for path in [b"/dev/null\0", b"/dev/zero\0"] {
            let mut payload = (path.len() as u16).to_ne_bytes().to_vec();
            payload.extend_from_slice(path);
            stream
                .write_all(&protocol::packet(protocol::CLIENT_OPEN_DEVICE, &payload).unwrap())
                .unwrap();
            if path == b"/dev/null\0" {
                assert_eq!(read_packet(&mut stream).0, protocol::SERVER_DEVICE_OPENED);
            }
        }
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        // Drop joins the failed actor, ensuring its failure event was queued.
        drop(handle);
        assert_eq!(*revoked.lock().unwrap(), 1);
        assert!(runtime.drain_events().iter().any(|event| matches!(event,
            SeatEvent::ActorFailed { message, .. } if message.contains("injected driver panic"))));
    }

    #[test]
    fn device_removal_is_acknowledged_after_revocation_and_still_accepts_close() {
        use std::os::unix::fs::MetadataExt;

        struct RecordingDeviceOps(Arc<Mutex<usize>>);
        impl DeviceOps for RecordingDeviceOps {
            fn open(&mut self, _device: &AllowedDevice) -> io::Result<OwnedFd> {
                Ok(std::fs::File::open("/dev/null")?.into())
            }
            fn deactivate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
                *self.0.lock().unwrap() += 1;
                Ok(())
            }
            fn activate(&mut self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
                panic!("removed device must not be reactivated")
            }
        }
        let runtime = ServerRuntime::initialize().unwrap();
        let revoked = Arc::new(Mutex::new(0));
        let handle = runtime
            .spawn_with_device_ops(
                SeatConfig {
                    name: "seat-test".into(),
                    vt_bound: false,
                },
                Box::new(RecordingDeviceOps(revoked.clone())),
            )
            .unwrap();
        let allowed = AllowedDevice::canonicalized(
            DeviceId::new("null"),
            "/dev/null",
            std::fs::metadata("/dev/null").unwrap().rdev(),
            DeviceKind::Evdev,
        )
        .unwrap();
        handle.replace_devices(vec![allowed.clone()]).unwrap();
        let mut stream = client(&handle, "client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_SEAT_OPENED);
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_ENABLE_SEAT);
        let path = b"/dev/null\0";
        let mut payload = (path.len() as u16).to_ne_bytes().to_vec();
        payload.extend_from_slice(path);
        let open = protocol::packet(protocol::CLIENT_OPEN_DEVICE, &payload).unwrap();
        stream.write_all(&open).unwrap();
        let (opcode, payload) = read_packet(&mut stream);
        assert_eq!(opcode, protocol::SERVER_DEVICE_OPENED);
        let device_id = protocol::parse_i32(&payload).unwrap();
        handle.remove_device(allowed.id).unwrap();
        assert_eq!(*revoked.lock().unwrap(), 1);
        stream.write_all(&open).unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_ERROR);
        stream
            .write_all(
                &protocol::packet(protocol::CLIENT_CLOSE_DEVICE, &device_id.to_ne_bytes()).unwrap(),
            )
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_DEVICE_CLOSED);
        assert_eq!(*revoked.lock().unwrap(), 1);
        handle.shutdown().unwrap();
    }

    #[test]
    fn invalid_seat_names_are_rejected_before_spawning() {
        let runtime = ServerRuntime::initialize().unwrap();
        for name in ["", "seat\0other"] {
            assert!(
                runtime
                    .spawn_seat(SeatConfig {
                        name: name.into(),
                        vt_bound: false
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn commands_racing_actor_exit_do_not_block() {
        let (_runtime, handle) = actor();
        let (reply, received) = sync_channel(1);
        {
            let mut queue = handle.mailbox.queue.lock().unwrap();
            queue.commands.push_back(Command::Shutdown(reply));
            wake_eventfd(&handle.mailbox.wake).unwrap();
        }
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        // This can race the actor's final cleanup. Either way the request must
        // promptly fail rather than remaining queued with a live reply sender.
        let (done, result) = sync_channel(1);
        let thread = std::thread::spawn(move || {
            done.send(handle.revoke_client(ClientId::new("missing")))
                .unwrap();
        });
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        thread.join().unwrap();
    }

    #[test]
    fn actor_supports_close_then_reopen_on_one_socket() {
        let (_runtime, handle) = actor();
        let mut stream = client(&handle, "client");
        stream
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_SEAT_OPENED);
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_ENABLE_SEAT);

        stream
            .write_all(&protocol::empty(protocol::CLIENT_CLOSE_SEAT))
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_SEAT_CLOSED);

        stream
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_SEAT_OPENED);
        assert_eq!(read_packet(&mut stream).0, protocol::SERVER_ENABLE_SEAT);
        handle.shutdown().unwrap();
    }

    #[test]
    fn injected_connections_are_isolated_per_seat_actor() {
        let runtime = ServerRuntime::initialize().unwrap();
        let first = runtime
            .spawn_with_device_ops(
                SeatConfig {
                    name: "seat-a".into(),
                    vt_bound: false,
                },
                Box::<FakeDeviceOps>::default(),
            )
            .unwrap();
        let second = runtime
            .spawn_with_device_ops(
                SeatConfig {
                    name: "seat-b".into(),
                    vt_bound: false,
                },
                Box::<FakeDeviceOps>::default(),
            )
            .unwrap();
        let mut first_client = client(&first, "first");
        let mut second_client = client(&second, "second");
        for stream in [&mut first_client, &mut second_client] {
            stream
                .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
                .unwrap();
        }
        let first_opened = read_packet(&mut first_client);
        let second_opened = read_packet(&mut second_client);
        assert_eq!(first_opened.0, protocol::SERVER_SEAT_OPENED);
        assert_eq!(second_opened.0, protocol::SERVER_SEAT_OPENED);
        assert!(first_opened.1.ends_with(b"seat-a\0"));
        assert!(second_opened.1.ends_with(b"seat-b\0"));
        assert_eq!(
            read_packet(&mut first_client).0,
            protocol::SERVER_ENABLE_SEAT
        );
        assert_eq!(
            read_packet(&mut second_client).0,
            protocol::SERVER_ENABLE_SEAT
        );
        first.shutdown().unwrap();
        second.shutdown().unwrap();
    }

    #[test]
    fn buffered_batches_do_not_starve_socket_output_or_commands() {
        let (_runtime, handle) = actor();
        let mut stream = client(&handle, "client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let writer = thread::spawn(move || {
            let packets = protocol::empty(protocol::CLIENT_PING).repeat(20_000);
            writer.write_all(&packets).unwrap();
        });
        for _ in 0..20_000 {
            assert_eq!(read_packet(&mut stream).0, protocol::SERVER_PONG);
        }
        writer.join().unwrap();
        handle.revoke_client(ClientId::new("client")).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn disconnect_during_enable_delivery_rolls_back_vt_activation() {
        let runtime = ServerRuntime::initialize().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let handle = runtime
            .spawn_with_ops(
                SeatConfig {
                    name: "seat0".into(),
                    vt_bound: true,
                },
                Box::<FakeDeviceOps>::default(),
                Some(Box::new(FakeVtOps {
                    current: 7,
                    events: events.clone(),
                })),
            )
            .unwrap();
        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        handle.add_client(ClientId::new("client"), server).unwrap();
        let mut client: std::os::unix::net::UnixStream = client.into();
        client
            .write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))
            .unwrap();
        drop(client);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = events.lock().unwrap().clone();
            if snapshot.contains(&"open:7".to_owned()) && snapshot.contains(&"close:7".to_owned()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "VT activation was not rolled back"
            );
            thread::sleep(Duration::from_millis(10));
        }
        handle.shutdown().unwrap();
    }

    #[test]
    fn eof_reports_client_gone() {
        let (runtime, handle) = actor();
        let stream = client(&handle, "client");
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if runtime.drain_events().into_iter().any(|event| {
                matches!(event, SeatEvent::ClientGone { client, .. } if client.as_str() == "client")
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "client-gone event was not emitted"
            );
            thread::sleep(Duration::from_millis(10));
        }
        handle.shutdown().unwrap();
    }

    #[test]
    fn explicit_revoke_is_idempotent() {
        let (_runtime, handle) = actor();
        let _stream = client(&handle, "client");
        handle.revoke_client(ClientId::new("client")).unwrap();
        handle.revoke_client(ClientId::new("client")).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn vt_bound_actor_initializes_its_signal_and_timer_loop() {
        let runtime = ServerRuntime::initialize().unwrap();
        let handle = runtime
            .spawn_seat(SeatConfig {
                name: "seat0".into(),
                vt_bound: true,
            })
            .unwrap();
        handle.shutdown().unwrap();
    }
}
