//! Private-bus integration fixture, never installed.
use dbus::blocking::Connection;
use dbus::channel::Sender;
use dbus::message::MatchRule;
use libmultiseatd::{BUS_NAME, Client, INTERFACE_NAME, OBJECT_PATH};
use seatd_server::protocol;
use std::error::Error;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

fn client() -> Result<Client, dbus::Error> {
    Client::system(Duration::from_secs(3))
}
fn packet(stream: &mut std::os::unix::net::UnixStream) -> u16 {
    let mut header = [0; 4];
    stream.read_exact(&mut header).unwrap();
    let mut body = vec![0; u16::from_ne_bytes([header[2], header[3]]) as usize];
    stream.read_exact(&mut body).unwrap();
    u16::from_ne_bytes([header[0], header[1]])
}
fn wait_until(mut check: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < end, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.get(1).map(String::as_str).unwrap_or("contract") {
        "ready" => {
            assert_eq!(client()?.list_seats()?, ["seat-test"]);
        }
        "deny" => {
            let bus = Connection::new_system()?;
            let proxy = bus.with_proxy(BUS_NAME, OBJECT_PATH, Duration::from_secs(3));
            let calls: Vec<Result<(), dbus::Error>> = vec![
                proxy.method_call(INTERFACE_NAME, "ListSeats", ()),
                proxy.method_call(INTERFACE_NAME, "ListClients", ()),
                proxy.method_call(INTERFACE_NAME, "CreateClient", ("seat-test", 1000u32)),
                proxy.method_call(
                    INTERFACE_NAME,
                    "RevokeClient",
                    ("01900000-0000-7000-8000-000000000000",),
                ),
                proxy.method_call("org.freedesktop.DBus.Introspectable", "Introspect", ()),
            ];
            for call in calls {
                assert!(
                    call.unwrap_err()
                        .name()
                        .unwrap_or_default()
                        .contains("AccessDenied")
                );
            }
        }
        "hold" => {
            let (_, fd) = client()?.create_client("seat-test", 1000)?;
            let mut stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_read_timeout(Some(Duration::from_secs(15)))?;
            std::fs::write(&args[2], b"ready")?;
            assert_eq!(
                stream.read(&mut [0])?,
                0,
                "bus loss must close client endpoints"
            );
        }
        "watch" => {
            let bus = Connection::new_system()?;
            let seen = Arc::new(AtomicBool::new(false));
            let record = seen.clone();
            bus.add_match::<(String,), _>(
                MatchRule::new_signal(INTERFACE_NAME, "SeatNew"),
                move |_, _, _| {
                    record.store(true, Ordering::SeqCst);
                    true
                },
            )?;
            std::fs::write(&args[2], b"ready")?;
            let end = Instant::now() + Duration::from_secs(2);
            while Instant::now() < end {
                bus.process(Duration::from_millis(50))?;
            }
            assert_eq!(seen.load(Ordering::SeqCst), args[3] == "yes");
        }
        "emit" => {
            let bus = Connection::new_system()?;
            bus.request_name(BUS_NAME, false, false, true)?;
            std::fs::write(&args[2], b"ready")?;
            wait_until(|| std::path::Path::new(&format!("{}.go", args[2])).exists());
            bus.send(
                dbus::Message::new_signal(OBJECT_PATH, INTERFACE_NAME, "SeatNew")?
                    .append1("seat-test"),
            )
            .map_err(|_| "signal send failed")?;
            bus.channel().flush();
            std::thread::sleep(Duration::from_millis(300));
        }
        "contract" => {
            let client = client()?;
            assert_eq!(client.list_seats()?, ["seat-test"]);
            assert!(client.list_clients()?.is_empty());
            let (id, fd) = client.create_client("seat-test", 1000)?;
            assert_eq!(uuid::Uuid::parse_str(&id)?.get_version_num(), 7);
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert_eq!(
                client.list_clients()?,
                [(id.clone(), "seat-test".into(), 1000, "connected".into())]
            );
            let retained = fd.try_clone()?;
            let mut stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.write_all(&protocol::empty(protocol::CLIENT_OPEN_SEAT))?;
            assert_eq!(packet(&mut stream), protocol::SERVER_SEAT_OPENED);
            assert_eq!(packet(&mut stream), protocol::SERVER_ENABLE_SEAT);
            wait_until(|| client.list_clients().unwrap()[0].3 == "active");
            stream.write_all(&protocol::empty(protocol::CLIENT_CLOSE_SEAT))?;
            assert_eq!(packet(&mut stream), protocol::SERVER_SEAT_CLOSED);
            wait_until(|| client.list_clients().unwrap()[0].3 == "connected");
            drop(stream);
            assert_eq!(
                client.list_clients()?.len(),
                1,
                "retained duplicate keeps lease alive"
            );
            drop(retained);
            wait_until(|| client.list_clients().unwrap().is_empty());
            client.revoke_client(&id)?;
            let (id, fd) = client.create_client("seat-test", 1001)?;
            client.revoke_client(&id)?;
            client.revoke_client(&id)?;
            let mut stream = std::os::unix::net::UnixStream::from(fd);
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            assert_eq!(stream.read(&mut [0])?, 0);
            assert!(client.create_client("seat-missing", 1000).is_err());
            assert!(client.revoke_client("invalid").is_err());
            let bus = Connection::new_system()?;
            let proxy = bus.with_proxy(BUS_NAME, OBJECT_PATH, Duration::from_secs(3));
            let removed: Result<(), _> = proxy.method_call(INTERFACE_NAME, "Refresh", ("", true));
            assert!(removed.is_err());
            let (xml,): (String,) =
                proxy.method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())?;
            assert!(xml.contains("a(ssus)"));
            assert!(!xml.contains("Refresh"));
        }
        _ => return Err("unknown probe mode".into()),
    }
    Ok(())
}
