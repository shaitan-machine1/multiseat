use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use seatd_server::{
    AllowedDevice, ClientId, DeviceId, DeviceKind, SeatConfig, SeatEvent, ServerRuntime,
};
use std::env;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const INHERITED_FD: RawFd = 57;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client_program = env::args_os()
        .nth(1)
        .ok_or("usage: inherited-libseat-server CLIENT")?;
    let runtime = ServerRuntime::initialize()?;
    let seat = runtime.spawn_seat(SeatConfig {
        name: "seat-test".into(),
        vt_bound: false,
    })?;
    let metadata = std::fs::metadata("/dev/null")?;
    seat.add_device(AllowedDevice::canonicalized(
        DeviceId::new("null"),
        "/dev/null",
        metadata.rdev(),
        DeviceKind::Evdev,
    )?)?;
    let (server, client) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )?;
    let client_id = ClientId::new("real-libseat");
    seat.add_client(client_id.clone(), server)?;

    let source_fd = client.as_raw_fd();
    let mut command = Command::new(client_program);
    command
        .env("SEATD_FD", INHERITED_FD.to_string())
        .env("LIBSEAT_BACKEND", "seatd");
    // SAFETY: this closure only performs async-signal-safe descriptor operations
    // between fork and exec, and all captured values are plain integers.
    unsafe {
        command.pre_exec(move || prepare_inherited_fd(source_fd, INHERITED_FD));
    }
    let mut child = command.spawn()?;
    drop(client);
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("real libseat client exited with {status}").into());
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if runtime.drain_events().into_iter().any(
            |event| matches!(event, SeatEvent::ClientGone { client, .. } if client == client_id),
        ) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("seatd actor did not observe final libseat EOF".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    seat.shutdown()?;
    Ok(())
}

fn prepare_inherited_fd(source: RawFd, target: RawFd) -> io::Result<()> {
    if source != target {
        // SAFETY: source is the live socketpair endpoint and dup2 atomically
        // installs a child-owned copy at target.
        if unsafe { libc::dup2(source, target) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: F_GETFD/F_SETFD operate only on the live target descriptor.
    let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(target, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
