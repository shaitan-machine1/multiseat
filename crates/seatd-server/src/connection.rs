use crate::protocol::{DecodeError, Decoder, MAX_BUFFER_LEN, Packet};
use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use std::collections::VecDeque;
use std::io::{self, IoSlice};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

const READ_SIZE: usize = 4096;
const MAX_RECEIVED_FDS: usize = 16;
const MAX_OUTBOUND_MESSAGES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadStatus {
    Progress,
    WouldBlock,
    Eof,
}

struct Outbound {
    bytes: Vec<u8>,
    offset: usize,
    fd: Option<OwnedFd>,
}

/// One nonblocking seatd stream with buffered framing and descriptor passing.
pub struct Connection {
    fd: OwnedFd,
    decoder: Decoder,
    received_fds: VecDeque<OwnedFd>,
    outgoing: VecDeque<Outbound>,
    eof: bool,
}

impl Connection {
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFL).map_err(errno_to_io)?;
        let mut flags = nix::fcntl::OFlag::from_bits_truncate(flags);
        flags.insert(nix::fcntl::OFlag::O_NONBLOCK);
        nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFL(flags)).map_err(errno_to_io)?;
        Ok(Self {
            fd,
            decoder: Decoder::default(),
            received_fds: VecDeque::new(),
            outgoing: VecDeque::new(),
            eof: false,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }

    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// Leave room for responses to an entire actor batch while a peer is slow.
    pub fn can_process_input(&self) -> bool {
        self.outgoing.len() < MAX_OUTBOUND_MESSAGES / 2
    }

    pub fn received_fd_count(&self) -> usize {
        self.received_fds.len()
    }

    pub fn take_received_fd(&mut self) -> Option<OwnedFd> {
        self.received_fds.pop_front()
    }

    pub fn clear_received_fds(&mut self) {
        self.received_fds.clear();
    }

    pub fn read(&mut self) -> io::Result<ReadStatus> {
        let mut bytes = [0u8; READ_SIZE];
        let capacity = MAX_BUFFER_LEN.saturating_sub(self.decoder.pending());
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "seatd input buffer is full",
            ));
        }
        let (count, received) = match receive_owned(&self.fd, &mut bytes[..capacity.min(READ_SIZE)])
        {
            Ok(message) => message,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(ReadStatus::WouldBlock);
            }
            Err(error) => return Err(error),
        };
        self.received_fds.extend(received);
        if count == 0 {
            self.eof = true;
            return Ok(ReadStatus::Eof);
        }
        self.decoder.push(&bytes[..count]).map_err(decode_to_io)?;
        Ok(ReadStatus::Progress)
    }

    pub fn next_packet(&mut self) -> io::Result<Option<Packet>> {
        self.decoder.decode_next().map_err(decode_to_io)
    }

    pub fn has_complete_packet(&self) -> io::Result<bool> {
        self.decoder.has_complete_packet().map_err(decode_to_io)
    }

    pub fn pending_input(&self) -> usize {
        self.decoder.pending()
    }

    pub fn queue(&mut self, bytes: Vec<u8>, fd: Option<OwnedFd>) -> io::Result<()> {
        if self.outgoing.len() >= MAX_OUTBOUND_MESSAGES {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "seatd client output queue is full",
            ));
        }
        self.outgoing.push_back(Outbound {
            bytes,
            offset: 0,
            fd,
        });
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<bool> {
        while let Some(outbound) = self.outgoing.front_mut() {
            let iov = [IoSlice::new(&outbound.bytes[outbound.offset..])];
            let sent = if let Some(fd) = outbound.fd.as_ref() {
                let fds = [fd.as_raw_fd()];
                sendmsg::<()>(
                    self.fd.as_raw_fd(),
                    &iov,
                    &[ControlMessage::ScmRights(&fds)],
                    MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_NOSIGNAL,
                    None,
                )
            } else {
                sendmsg::<()>(
                    self.fd.as_raw_fd(),
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
                        "seatd socket closed while writing",
                    ));
                }
                Ok(count) => {
                    outbound.offset += count;
                    // SCM_RIGHTS is delivered with the first successfully
                    // written byte and must not be attached again after a
                    // partial stream write.
                    outbound.fd.take();
                    if outbound.offset == outbound.bytes.len() {
                        self.outgoing.pop_front();
                    }
                }
                Err(Errno::EAGAIN) => return Ok(false),
                Err(error) => return Err(errno_to_io(error)),
            }
        }
        Ok(true)
    }
}

/// Unlike nix's cmsgs(), this owns installed SCM_RIGHTS descriptors even when
/// MSG_CTRUNC is set. Linux closes the excess descriptors that did not fit; we
/// must close those that DID fit before rejecting the message.
fn receive_owned(fd: &OwnedFd, bytes: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    // cmsghdr alignment is required by CMSG_FIRSTHDR/NXTHDR.
    let mut control = [0usize;
        (size_of::<libc::cmsghdr>() + MAX_RECEIVED_FDS * size_of::<i32>())
            .div_ceil(size_of::<usize>())];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: all remaining msghdr fields are nullable/zero-length outputs.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control);
    // SAFETY: iov and control describe live writable buffers for this call.
    let count = unsafe {
        libc::recvmsg(
            fd.as_raw_fd(),
            &mut message,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut received = Vec::new();
    // SAFETY: recvmsg produced the kernel-validated ancillary headers. We still
    // bound each header and payload to the returned buffer before reading it.
    unsafe {
        let end = (message.msg_control as usize) + message.msg_controllen;
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let start = header as usize;
            if start + size_of::<libc::cmsghdr>() > end {
                break;
            }
            let length = (*header).cmsg_len;
            let minimum = libc::CMSG_LEN(0) as usize;
            if length < minimum || length > end - start {
                break;
            }
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<i32>();
                for index in 0..(length - minimum) / size_of::<i32>() {
                    received.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated seatd ancillary data",
        ));
    }
    Ok((count as usize, received))
}

fn errno_to_io(error: Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn decode_to_io(error: DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::io::Write;
    use std::os::fd::IntoRawFd;

    fn pair() -> (Connection, std::os::unix::net::UnixStream) {
        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )
        .unwrap();
        let server = Connection::new(server).unwrap();
        // SAFETY: ownership is transferred out of the OwnedFd exactly once.
        let client = unsafe { std::os::unix::net::UnixStream::from_raw_fd(client.into_raw_fd()) };
        client.set_nonblocking(false).unwrap();
        (server, client)
    }

    #[test]
    fn fragmented_packets_and_terminal_eof_are_preserved() {
        let (mut connection, mut peer) = pair();
        let packet = protocol::empty(protocol::CLIENT_PING);
        peer.write_all(&packet[..2]).unwrap();
        assert_eq!(connection.read().unwrap(), ReadStatus::Progress);
        assert!(connection.next_packet().unwrap().is_none());
        peer.write_all(&packet[2..]).unwrap();
        assert_eq!(connection.read().unwrap(), ReadStatus::Progress);
        assert_eq!(
            connection.next_packet().unwrap().unwrap().opcode,
            protocol::CLIENT_PING
        );
        drop(peer);
        assert_eq!(connection.read().unwrap(), ReadStatus::Eof);
        assert!(connection.is_eof());
    }

    #[test]
    fn terminal_eof_preserves_a_buffered_complete_packet() {
        let (mut connection, mut peer) = pair();
        peer.write_all(&protocol::empty(protocol::CLIENT_PING))
            .unwrap();
        drop(peer);
        assert_eq!(connection.read().unwrap(), ReadStatus::Progress);
        assert_eq!(connection.read().unwrap(), ReadStatus::Eof);
        assert!(connection.is_eof());
        assert_eq!(
            connection.next_packet().unwrap().unwrap().opcode,
            protocol::CLIENT_PING
        );
        assert!(connection.next_packet().unwrap().is_none());
    }

    #[test]
    fn queued_descriptors_are_sent_once_and_received_close_on_exec() {
        let (left, right) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        )
        .unwrap();
        let mut sender = Connection::new(left).unwrap();
        let mut receiver = Connection::new(right).unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        sender
            .queue(protocol::device_opened(7), Some(OwnedFd::from(file)))
            .unwrap();
        assert!(sender.flush().unwrap());
        assert_eq!(receiver.read().unwrap(), ReadStatus::Progress);
        assert_eq!(receiver.received_fd_count(), 1);
        assert_eq!(
            receiver.next_packet().unwrap().unwrap(),
            Packet {
                opcode: protocol::SERVER_DEVICE_OPENED,
                payload: 7i32.to_ne_bytes().to_vec()
            }
        );
        let received = receiver.take_received_fd().unwrap();
        let flags = nix::fcntl::fcntl(&received, nix::fcntl::FcntlArg::F_GETFD).unwrap();
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn outbound_queue_is_bounded() {
        let (mut connection, _peer) = pair();
        for _ in 0..MAX_OUTBOUND_MESSAGES {
            connection
                .queue(protocol::empty(protocol::SERVER_PONG), None)
                .unwrap();
        }
        assert_eq!(
            connection
                .queue(protocol::empty(protocol::SERVER_PONG), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::OutOfMemory
        );
    }

    #[test]
    fn malformed_input_drops_received_descriptors_without_leaking() {
        let (mut connection, peer) = pair();
        let (read_end, write_end) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC | nix::fcntl::OFlag::O_NONBLOCK)
                .unwrap();
        let mut header = protocol::CLIENT_OPEN_DEVICE.to_ne_bytes().to_vec();
        header.extend_from_slice(&((protocol::MAX_PAYLOAD_LEN + 1) as u16).to_ne_bytes());
        let iov = [IoSlice::new(&header)];
        let rights = [write_end.as_raw_fd()];
        sendmsg::<()>(
            peer.as_raw_fd(),
            &iov,
            &[ControlMessage::ScmRights(&rights)],
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
        drop(write_end);

        assert_eq!(connection.read().unwrap(), ReadStatus::Progress);
        assert_eq!(connection.received_fd_count(), 1);
        assert_eq!(
            connection.next_packet().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        drop(connection);

        let mut byte = 0u8;
        // SAFETY: byte is a valid one-byte output buffer and read_end is live.
        assert_eq!(
            unsafe { libc::read(read_end.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
    }

    #[test]
    fn truncated_ancillary_data_closes_every_installed_descriptor() {
        let (mut connection, peer) = pair();
        let (read_end, write_end) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC | nix::fcntl::OFlag::O_NONBLOCK)
                .unwrap();
        let bytes = protocol::empty(protocol::CLIENT_PING);
        let rights = vec![write_end.as_raw_fd(); MAX_RECEIVED_FDS * 2];
        sendmsg::<()>(
            peer.as_raw_fd(),
            &[IoSlice::new(&bytes)],
            &[ControlMessage::ScmRights(&rights)],
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .unwrap();
        drop(write_end);
        assert_eq!(
            connection.read().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        drop(connection);
        drop(peer);
        // Any leaked copy of the pipe writer would yield EAGAIN instead of EOF.
        assert_eq!(nix::unistd::read(&read_end, &mut [0]).unwrap(), 0);
    }

    #[test]
    fn malformed_oversized_input_is_rejected() {
        let (mut connection, mut peer) = pair();
        let mut header = protocol::CLIENT_OPEN_DEVICE.to_ne_bytes().to_vec();
        header.extend_from_slice(&((protocol::MAX_PAYLOAD_LEN + 1) as u16).to_ne_bytes());
        peer.write_all(&header).unwrap();
        connection.read().unwrap();
        assert_eq!(
            connection.next_packet().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
