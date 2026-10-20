use multiseatd::authority::{AuthorizedDevice, DeviceAuthority, OpenError};
use multiseatd::broker::Broker;
use multiseatd::connection;
use multiseatd::device::DeviceOps;
use multiseatd::protocol::{self, Decoder};
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use rustix::process::{Pid, PidfdFlags, pidfd_open};
use seat_udev::{DeviceKind, SeatName};
use std::cell::RefCell;
use std::io::{self, IoSlice};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

struct NoDevices;
impl DeviceAuthority for NoDevices {
    fn authorize(
        &self,
        _seat: &SeatName,
        _requested: &Path,
    ) -> Result<AuthorizedDevice, OpenError> {
        Err(OpenError(libc::EPERM))
    }
}

struct NoOps;
impl DeviceOps for NoOps {
    fn open(&self, _device: &AuthorizedDevice) -> io::Result<OwnedFd> {
        unreachable!()
    }
    fn revoke(&self, _kind: DeviceKind, _fd: &OwnedFd) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn opener_pidfd_ends_client_even_while_socket_copy_remains_open() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let seat = SeatName::parse("seat1").unwrap();
            let mut broker = Broker::new(Rc::new(NoDevices), Rc::new(NoOps));
            broker.add_seat(seat.clone(), false, None);
            let broker = Rc::new(RefCell::new(broker));
            let (server, mut client) = UnixStream::pair().unwrap();
            tokio::task::spawn_local(connection::serve(server, seat, broker));

            let mut opener = Command::new("sleep")
                .arg("300")
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let pid = Pid::from_raw(opener.id() as i32).unwrap();
            let pidfd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
            let request = protocol::empty(protocol::CLIENT_OPEN_SEAT);
            let iov = [IoSlice::new(&request)];
            let fds = [pidfd.as_raw_fd()];
            sendmsg::<()>(
                client.as_raw_fd(),
                &iov,
                &[ControlMessage::ScmRights(&fds)],
                MsgFlags::MSG_NOSIGNAL,
                None,
            )
            .unwrap();

            let mut bytes = [0u8; 64];
            let mut decoder = Decoder::default();
            let mut opcodes = Vec::new();
            while opcodes.len() < 2 {
                let count = tokio::time::timeout(Duration::from_secs(2), client.read(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap();
                decoder.push(&bytes[..count]);
                while let Some(packet) = decoder.decode_next() {
                    opcodes.push(packet.opcode);
                }
            }
            assert_eq!(
                opcodes,
                [protocol::SERVER_SEAT_OPENED, protocol::SERVER_ENABLE_SEAT]
            );

            opener.kill().unwrap();
            opener.wait().unwrap();
            let count = tokio::time::timeout(Duration::from_secs(2), client.read(&mut bytes))
                .await
                .expect("pidfd should wake the server")
                .unwrap();
            assert_eq!(count, 0, "server must close despite this live socket copy");
        })
        .await;
}
