use clap::{Parser, ValueEnum};
use multiseatd::authority::ContextAuthority;
use multiseatd::broker::Broker;
use multiseatd::connection;
use multiseatd::dbus::{BUS_NAME, Manager, OBJECT_PATH, SeatRegistry};
use multiseatd::device::RealDeviceOps;
use multiseatd::vt::LinuxVtOps;
use seat_udev::{Context, Event, SeatName};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use zbus::object_server::SignalEmitter;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BusKind {
    System,
    Session,
}

#[derive(Debug, Parser)]
#[command(version, about = "Central udev-aware seatd-compatible broker")]
struct Args {
    #[arg(long, default_value = "/run/multiseatd")]
    runtime_dir: PathBuf,
    #[arg(long, value_enum, default_value_t = BusKind::System)]
    bus: BusKind,
    /// Add a non-VT seat independently of udev (local protocol testing only).
    #[arg(long, hide = true)]
    test_seat: Vec<SeatName>,
}

struct Unit {
    task: JoinHandle<()>,
    socket: PathBuf,
}

struct Units {
    runtime_dir: PathBuf,
    broker: Rc<RefCell<Broker>>,
    units: HashMap<SeatName, Unit>,
}

impl Units {
    fn new(runtime_dir: PathBuf, broker: Rc<RefCell<Broker>>) -> Self {
        Self {
            runtime_dir,
            broker,
            units: HashMap::new(),
        }
    }

    async fn add(&mut self, seat: SeatName, vt_bound: bool) -> std::io::Result<bool> {
        if self.units.contains_key(&seat) {
            return Ok(false);
        }
        let socket = self.runtime_dir.join(format!("{seat}.sock"));
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let current_vt = if vt_bound { current_vt() } else { None };
        self.broker
            .borrow_mut()
            .add_seat(seat.clone(), vt_bound, current_vt);
        let broker = self.broker.clone();
        let task_seat = seat.clone();
        let task = tokio::task::spawn_local(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::task::spawn_local(connection::serve(
                            stream,
                            task_seat.clone(),
                            broker.clone(),
                        ));
                    }
                    Err(error) => {
                        eprintln!("multiseatd: accept on {task_seat} failed: {error}");
                        break;
                    }
                }
            }
        });
        self.units.insert(seat, Unit { task, socket });
        Ok(true)
    }

    fn remove(&mut self, seat: &SeatName) -> bool {
        let Some(unit) = self.units.remove(seat) else {
            return false;
        };
        unit.task.abort();
        self.broker.borrow_mut().remove_seat(seat);
        let _ = fs::remove_file(unit.socket);
        true
    }
}

impl Drop for Units {
    fn drop(&mut self) {
        for (_, unit) in self.units.drain() {
            unit.task.abort();
            let _ = fs::remove_file(unit.socket);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, run(Args::parse()))
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(&args.runtime_dir)?;
    fs::set_permissions(&args.runtime_dir, fs::Permissions::from_mode(0o700))?;

    let context = Rc::new(RefCell::new(Context::new()?));
    let authority = Rc::new(ContextAuthority::new(context.clone()));
    let broker = Rc::new(RefCell::new(Broker::with_vt(
        authority,
        Rc::new(RealDeviceOps),
        Rc::new(LinuxVtOps),
    )));
    let mut units = Units::new(args.runtime_dir.clone(), broker.clone());

    let builder = match args.bus {
        BusKind::System => zbus::connection::Builder::system(),
        BusKind::Session => zbus::connection::Builder::session(),
    };
    let registry = SeatRegistry::default();
    let connection = builder.build().await?;
    connection
        .object_server()
        .at(OBJECT_PATH, Manager::new(registry.clone()))
        .await?;
    // Not zbus's default flags (ALLOW_REPLACEMENT | REPLACE_EXISTING |
    // DO_NOT_QUEUE): a second instance must not be able to take the name
    // away from the daemon that owns the seat sockets.
    match connection
        .request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DO_NOT_QUEUE)
        .await
    {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => {
            return Err(format!(
                "{BUS_NAME} is already owned on this bus; another multiseatd is running"
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    }
    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?;

    let initial_seats = context.borrow().seats();
    for seat in initial_seats.into_iter().chain(args.test_seat) {
        let added = units.add(seat.clone(), seat == SeatName::seat0()).await?;
        if added {
            registry.insert(seat.as_str());
        }
    }

    let seat0 = SeatName::seat0();
    let mut release =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut acquire =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;
    let mut interval = tokio::time::interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            // Without the bus we can no longer publish seats, so die and
            // let the supervisor restart us with a fresh connection.
            _ = connection.closed() => return Err("lost the D-Bus connection".into()),
            _ = release.recv() => {
                broker.borrow_mut().vt_release(&seat0);
                acknowledge_vt(true);
            }
            _ = acquire.recv() => {
                acknowledge_vt(false);
                if let Some(vt) = current_vt() {
                    broker.borrow_mut().vt_acquire(&seat0, vt);
                }
            }
            _ = interval.tick() => {
                loop {
                    let events = context.borrow_mut().receive()?;
                    let Some(events) = events else { break };
                    for event in events {
                        match event {
                            Event::DeviceRemoved(device) => {
                                if let Some(path) = device.devnode {
                                    broker.borrow_mut().revoke_path(&device.seat, &path);
                                }
                            }
                            Event::DeviceMoved { device, from } => {
                                if let Some(path) = device.devnode {
                                    broker.borrow_mut().revoke_path(&from, &path);
                                }
                            }
                            Event::SeatAdded(seat) => {
                                if units.add(seat.clone(), seat == seat0).await? {
                                    registry.insert(seat.as_str());
                                    Manager::seat_new(&emitter, seat.as_str()).await?;
                                }
                            }
                            Event::SeatRemoved(seat) => {
                                if units.remove(&seat) {
                                    registry.remove(seat.as_str());
                                    Manager::seat_removed(&emitter, seat.as_str()).await?;
                                }
                            }
                            Event::DeviceAdded(_) => {}
                        }
                    }
                }
            }
        }
    }
    drop(units);
    remove_dir_if_empty(&args.runtime_dir);
    Ok(())
}

fn current_vt() -> Option<i32> {
    #[repr(C)]
    #[derive(Default)]
    struct VtStat {
        active: u16,
        signal: u16,
        state: u16,
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty0")
        .ok()?;
    let mut state = VtStat::default();
    // VT_GETSTATE from linux/vt.h.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x5603, &mut state) } == -1 {
        None
    } else {
        Some(i32::from(state.active))
    }
}

fn acknowledge_vt(release: bool) {
    let Ok(file) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty0")
    else {
        return;
    };
    // VT_RELDISP: 1 acknowledges release, VT_ACKACQ (2) acknowledges acquire.
    unsafe {
        libc::ioctl(file.as_raw_fd(), 0x5605, if release { 1 } else { 2 });
    }
}

fn remove_dir_if_empty(path: &Path) {
    let _ = fs::remove_dir(path);
}
