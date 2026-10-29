use crate::protocol::{DecodeError, Decoder, MAX_BUFFER_LEN, Packet};
use nix::errno::Errno;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut};
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
        let mut iov = [IoSliceMut::new(&mut bytes[..capacity.min(READ_SIZE)])];
        let mut cmsg = nix::cmsg_space!([i32; MAX_RECEIVED_FDS]);
        let message = match recvmsg::<()>(
            self.fd.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_CMSG_CLOEXEC,
        ) {
            Ok(message) => message,
            Err(Errno::EAGAIN) => return Ok(ReadStatus::WouldBlock),
            Err(error) => return Err(errno_to_io(error)),
        };
        let count = message.bytes;
        let flags = message.flags;
        let mut received = Vec::new();
        for item in message.cmsgs().map_err(errno_to_io)? {
            if let ControlMessageOwned::ScmRights(fds) = item {
                for fd in fds {
                    // SAFETY: SCM_RIGHTS installed a new descriptor owned by
                    // this process and MSG_CMSG_CLOEXEC marked it close-on-exec.
                    received.push(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
        if flags.contains(MsgFlags::MSG_CTRUNC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated seatd ancillary data",
            ));
        }
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
