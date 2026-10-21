use crate::broker::{Broker, ClientId, Outbound};
use crate::protocol::{self, Decoder, Packet};
use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use seat_udev::SeatName;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub async fn serve(stream: UnixStream, seat: SeatName, broker: Rc<RefCell<Broker>>) {
    let client_id = broker.borrow_mut().allocate_client();
    tracing::debug!(client = client_id, seat = %seat, "client connected");
    if let Err(error) = serve_inner(stream, seat, broker.clone(), client_id).await {
        tracing::warn!(client = client_id, %error, "client failed");
    }
    broker.borrow_mut().close_client(client_id);
}

async fn serve_inner(
    stream: UnixStream,
    seat: SeatName,
    broker: Rc<RefCell<Broker>>,
    client_id: ClientId,
) -> io::Result<()> {
    let std_stream = stream.into_std()?;
    std_stream.set_nonblocking(true)?;
    // SAFETY: OwnedFd remains valid and stable for the AsyncFd lifetime.
    let socket = unsafe { AsyncFd::register(OwnedFd::from(std_stream)) }
        .map_err(|error| error.into_parts().1)?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut decoder = Decoder::default();
    let mut received_fds = VecDeque::new();
    let mut ever_opened = false;

    loop {
        tokio::select! {
            readable = socket.readable() => {
                let mut guard = readable?;
                match receive(socket.get_ref().as_raw_fd(), &mut decoder, &mut received_fds) {
                    Ok(0) => break,
                    Ok(_) => guard.clear_ready(),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => guard.clear_ready(),
                    Err(error) => return Err(error),
                }
                while let Some(packet) = decoder.decode_next() {
                    let keep = handle_packet(
                        &socket,
                        packet,
                        &seat,
                        &broker,
                        client_id,
                        tx.clone(),
                        &mut received_fds,
                        &mut ever_opened,
                    ).await?;
                    if !keep {
                        return Ok(());
                    }
                }
            }
            outbound = rx.recv() => {
                match outbound {
                    Some(Outbound::Packet(bytes, fd)) => {
                        send_packet(&socket, &bytes, fd.as_ref()).await?;
                    }
                    Some(Outbound::ArmDisableTimeout) => {
                        let broker = broker.clone();
                        tokio::task::spawn_local(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            broker.borrow_mut().force_disable(client_id);
                        });
                    }
                    Some(Outbound::Disconnect) | None => return Ok(()),
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_packet(
    socket: &AsyncFd<OwnedFd>,
    packet: Packet,
    seat: &SeatName,
    broker: &Rc<RefCell<Broker>>,
    client_id: ClientId,
    tx: mpsc::UnboundedSender<Outbound>,
    received_fds: &mut VecDeque<OwnedFd>,
    ever_opened: &mut bool,
) -> io::Result<bool> {
    match packet.opcode {
        protocol::CLIENT_OPEN_SEAT => {
            if !packet.payload.is_empty() || *ever_opened {
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            *ever_opened = true;
            if received_fds.len() > 1 {
                received_fds.clear();
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            let opener = received_fds.pop_front();
            if opener.as_ref().is_some_and(|fd| !is_pidfd(fd.as_raw_fd())) {
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            let open_result = { broker.borrow_mut().open_seat(client_id, seat, tx.clone()) };
            let opened = match open_result {
                Ok(opened) => opened,
                Err(error) => {
                    send_error(socket, error).await?;
                    return Ok(true);
                }
            };
            send_packet(socket, &protocol::seat_opened(seat.as_str())?, None).await?;
            if opened.active {
                send_packet(socket, &protocol::empty(protocol::SERVER_ENABLE_SEAT), None).await?;
            }
            if let Some(pidfd) = opener {
                watch_opener(pidfd, tx);
            } else {
                tracing::info!(
                    client = client_id,
                    seat = %seat,
                    "client supplied no opener handle; using socket lifetime"
                );
            }
        }
        protocol::CLIENT_CLOSE_SEAT => {
            if !packet.payload.is_empty() {
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            broker.borrow_mut().close_client(client_id);
            send_packet(socket, &protocol::empty(protocol::SERVER_SEAT_CLOSED), None).await?;
        }
        protocol::CLIENT_OPEN_DEVICE => {
            let path = match protocol::parse_path(&packet.payload) {
                Ok(path) => path,
                Err(error) => {
                    send_error(socket, error).await?;
                    return Ok(false);
                }
            };
            let open_result = { broker.borrow_mut().open_device(client_id, Path::new(path)) };
            match open_result {
                Ok(opened) => {
                    send_packet(
                        socket,
                        &protocol::device_opened(opened.id),
                        Some(&opened.fd),
                    )
                    .await?;
                }
                Err(error) => send_error(socket, error).await?,
            }
        }
        protocol::CLIENT_CLOSE_DEVICE => {
            let id = match protocol::parse_i32(&packet.payload) {
                Ok(id) => id,
                Err(error) => {
                    send_error(socket, error).await?;
                    return Ok(false);
                }
            };
            let close_result = { broker.borrow_mut().close_device(client_id, id) };
            match close_result {
                Ok(()) => {
                    send_packet(
                        socket,
                        &protocol::empty(protocol::SERVER_DEVICE_CLOSED),
                        None,
                    )
                    .await?
                }
                Err(error) => send_error(socket, error).await?,
            }
        }
        protocol::CLIENT_DISABLE_SEAT => {
            if !packet.payload.is_empty() {
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            let acknowledge_result = { broker.borrow_mut().acknowledge_disable(client_id) };
            match acknowledge_result {
                Ok(()) => {
                    send_packet(
                        socket,
                        &protocol::empty(protocol::SERVER_SEAT_DISABLED),
                        None,
                    )
                    .await?
                }
                Err(error) => send_error(socket, error).await?,
            }
        }
        protocol::CLIENT_SWITCH_SESSION => {
            let session = match protocol::parse_i32(&packet.payload) {
                Ok(session) => session,
                Err(error) => {
                    send_error(socket, error).await?;
                    return Ok(false);
                }
            };
            let switch_result = { broker.borrow_mut().switch_session(client_id, session) };
            match switch_result {
                Ok(()) => {
                    send_packet(
                        socket,
                        &protocol::empty(protocol::SERVER_SESSION_SWITCHED),
                        None,
                    )
                    .await?
                }
                Err(error) => send_error(socket, error).await?,
            }
        }
        protocol::CLIENT_PING => {
            if !packet.payload.is_empty() {
                send_error(socket, libc::EBADMSG).await?;
                return Ok(false);
            }
            send_packet(socket, &protocol::empty(protocol::SERVER_PONG), None).await?;
        }
        _ => {
            send_error(socket, libc::EBADMSG).await?;
            return Ok(false);
        }
    }
    Ok(true)
}

fn receive(
    fd: RawFd,
    decoder: &mut Decoder,
    received_fds: &mut VecDeque<OwnedFd>,
) -> io::Result<usize> {
    let mut bytes = [0u8; 8192];
    let mut iov = [IoSliceMut::new(&mut bytes)];
    let mut cmsg = nix::cmsg_space!([RawFd; 8]);
    let message = recvmsg::<()>(
        fd,
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(errno_to_io)?;
    let count = message.bytes;
    for item in message.cmsgs().map_err(errno_to_io)? {
        if let ControlMessageOwned::ScmRights(fds) = item {
            for fd in fds {
                // SAFETY: SCM_RIGHTS installed a new descriptor owned by this
                // process; MSG_CMSG_CLOEXEC already marked it close-on-exec.
                received_fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    decoder.push(&bytes[..count]);
    Ok(count)
}

async fn send_error(socket: &AsyncFd<OwnedFd>, error: i32) -> io::Result<()> {
    send_packet(socket, &protocol::error(error), None).await
}

async fn send_packet(
    socket: &AsyncFd<OwnedFd>,
    bytes: &[u8],
    fd: Option<&OwnedFd>,
) -> io::Result<()> {
    let mut offset = 0;
    let mut attach_fd = fd;
    while offset < bytes.len() {
        let mut guard = socket.writable().await?;
        let iov = [IoSlice::new(&bytes[offset..])];
        let sent = if let Some(fd) = attach_fd {
            let raw_fds = [fd.as_raw_fd()];
            let cmsgs = [ControlMessage::ScmRights(&raw_fds)];
            sendmsg::<()>(
                socket.get_ref().as_raw_fd(),
                &iov,
                &cmsgs,
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                None,
            )
        } else {
            sendmsg::<()>(
                socket.get_ref().as_raw_fd(),
                &iov,
                &[],
                MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                None,
            )
        };
        match sent {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "seat socket closed",
                ));
            }
            Ok(written) => {
                offset += written;
                attach_fd = None;
            }
            Err(Errno::EAGAIN) => guard.clear_ready(),
            Err(error) => return Err(errno_to_io(error)),
        }
    }
    Ok(())
}

fn is_pidfd(fd: RawFd) -> bool {
    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
        .map(|info| info.lines().any(|line| line.starts_with("Pid:\t")))
        .unwrap_or(false)
}

fn watch_opener(pidfd: OwnedFd, tx: mpsc::UnboundedSender<Outbound>) {
    tokio::task::spawn_local(async move {
        // SAFETY: OwnedFd remains valid and stable for the AsyncFd lifetime.
        let Ok(pidfd) = (unsafe { AsyncFd::register(pidfd) }) else {
            let _ = tx.send(Outbound::Disconnect);
            return;
        };
        if pidfd.readable().await.is_ok() {
            let _ = tx.send(Outbound::Disconnect);
        }
    });
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
