use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use thiserror::Error;

pub const MAX_PATH_LEN: usize = 256;
pub const MAX_SEAT_LEN: usize = 64;
pub const MAX_PAYLOAD_LEN: usize = 2 + MAX_PATH_LEN;
pub const MAX_BUFFER_LEN: usize = 4096;

pub const CLIENT_OPEN_SEAT: u16 = 1;
pub const CLIENT_CLOSE_SEAT: u16 = 2;
pub const CLIENT_OPEN_DEVICE: u16 = 3;
pub const CLIENT_CLOSE_DEVICE: u16 = 4;
pub const CLIENT_DISABLE_SEAT: u16 = 5;
pub const CLIENT_SWITCH_SESSION: u16 = 6;
pub const CLIENT_PING: u16 = 7;

pub const SERVER_SEAT_OPENED: u16 = 0x8001;
pub const SERVER_SEAT_CLOSED: u16 = 0x8002;
pub const SERVER_DEVICE_OPENED: u16 = 0x8003;
pub const SERVER_DEVICE_CLOSED: u16 = 0x8004;
pub const SERVER_DISABLE_SEAT: u16 = 0x8005;
pub const SERVER_ENABLE_SEAT: u16 = 0x8006;
pub const SERVER_PONG: u16 = 0x8007;
pub const SERVER_SESSION_SWITCHED: u16 = 0x8008;
pub const SERVER_SEAT_DISABLED: u16 = 0x8009;
pub const SERVER_ERROR: u16 = 0xffff;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    pub opcode: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DecodeError {
    #[error("seatd connection input exceeded {MAX_BUFFER_LEN} bytes")]
    BufferOverflow,
    #[error("seatd packet payload of {0} bytes exceeds the protocol maximum")]
    PayloadTooLarge(usize),
}

#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_BUFFER_LEN {
            return Err(DecodeError::BufferOverflow);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn decode_next(&mut self) -> Result<Option<Packet>, DecodeError> {
        if self.bytes.len() < 4 {
            return Ok(None);
        }
        let opcode = u16::from_ne_bytes([self.bytes[0], self.bytes[1]]);
        let size = u16::from_ne_bytes([self.bytes[2], self.bytes[3]]) as usize;
        if size > MAX_PAYLOAD_LEN {
            return Err(DecodeError::PayloadTooLarge(size));
        }
        if self.bytes.len() < 4 + size {
            return Ok(None);
        }
        let payload = self.bytes[4..4 + size].to_vec();
        self.bytes.drain(..4 + size);
        Ok(Some(Packet { opcode, payload }))
    }

    pub fn pending(&self) -> usize {
        self.bytes.len()
    }

    pub fn has_complete_packet(&self) -> Result<bool, DecodeError> {
        if self.bytes.len() < 4 {
            return Ok(false);
        }
        let size = u16::from_ne_bytes([self.bytes[2], self.bytes[3]]) as usize;
        if size > MAX_PAYLOAD_LEN {
            return Err(DecodeError::PayloadTooLarge(size));
        }
        Ok(self.bytes.len() >= 4 + size)
    }
}

pub fn packet(opcode: u16, payload: &[u8]) -> io::Result<Vec<u8>> {
    let size = u16::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seatd payload too large"))?;
    let mut bytes = Vec::with_capacity(4 + payload.len());
    bytes.extend_from_slice(&opcode.to_ne_bytes());
    bytes.extend_from_slice(&size.to_ne_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

pub fn empty(opcode: u16) -> Vec<u8> {
    packet(opcode, &[]).expect("an empty packet always fits")
}

pub fn error(errno: i32) -> Vec<u8> {
    packet(SERVER_ERROR, &errno.to_ne_bytes()).expect("errno packet always fits")
}

pub fn seat_opened(name: &str) -> io::Result<Vec<u8>> {
    let valid = name.starts_with("seat")
        && name
            .as_bytes()
            .iter()
            .skip(4)
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seat name must start with 'seat' and contain only ASCII alphanumerics, '_' or '-'",
        ));
    }
    let name = format!("{name}\0");
    if name.len() > MAX_SEAT_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "seat name exceeds seatd protocol limit",
        ));
    }
    let mut payload = Vec::with_capacity(2 + name.len());
    payload.extend_from_slice(&(name.len() as u16).to_ne_bytes());
    payload.extend_from_slice(name.as_bytes());
    packet(SERVER_SEAT_OPENED, &payload)
}

pub fn device_opened(id: i32) -> Vec<u8> {
    packet(SERVER_DEVICE_OPENED, &id.to_ne_bytes()).expect("device packet always fits")
}

pub fn parse_empty(payload: &[u8]) -> Result<(), i32> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(libc::EBADMSG)
    }
}

pub fn parse_path(payload: &[u8]) -> Result<PathBuf, i32> {
    if payload.len() < 2 {
        return Err(libc::EBADMSG);
    }
    let len = u16::from_ne_bytes([payload[0], payload[1]]) as usize;
    let path = payload.get(2..).ok_or(libc::EBADMSG)?;
    if len == 0
        || len > MAX_PATH_LEN
        || path.len() != len
        || path.last() != Some(&0)
        || path[..len - 1].contains(&0)
    {
        return Err(libc::EBADMSG);
    }
    Ok(PathBuf::from(OsString::from_vec(path[..len - 1].to_vec())))
}

pub fn parse_i32(payload: &[u8]) -> Result<i32, i32> {
    if payload.len() != size_of::<i32>() {
        return Err(libc::EBADMSG);
    }
    Ok(i32::from_ne_bytes(
        payload.try_into().expect("length was checked"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn seat_names_are_validated_before_encoding() {
        assert!(seat_opened("").is_err());
        assert!(seat_opened("seat\0other").is_err());
        assert!(seat_opened("other0").is_err());
        assert!(seat_opened("seat/a").is_err());
        assert!(seat_opened(&format!("seat{}", "x".repeat(MAX_SEAT_LEN))).is_err());
        assert!(seat_opened(&format!("seat{}", "x".repeat(MAX_SEAT_LEN - 6))).is_ok());
    }

    #[test]
    fn decoder_handles_fragmented_and_coalesced_native_wire_packets() {
        let mut decoder = Decoder::default();
        let mut bytes = empty(CLIENT_OPEN_SEAT);
        bytes.extend(empty(CLIENT_PING));
        decoder.push(&bytes[..3]).unwrap();
        assert!(decoder.decode_next().unwrap().is_none());
        decoder.push(&bytes[3..]).unwrap();
        assert_eq!(
            decoder.decode_next().unwrap().unwrap(),
            Packet {
                opcode: CLIENT_OPEN_SEAT,
                payload: Vec::new()
            }
        );
        assert_eq!(decoder.decode_next().unwrap().unwrap().opcode, CLIENT_PING);
        assert!(decoder.decode_next().unwrap().is_none());
    }

    #[test]
    fn decoder_rejects_oversized_frames_before_waiting_for_the_payload() {
        let mut decoder = Decoder::default();
        let mut header = CLIENT_OPEN_DEVICE.to_ne_bytes().to_vec();
        header.extend_from_slice(&((MAX_PAYLOAD_LEN + 1) as u16).to_ne_bytes());
        decoder.push(&header).unwrap();
        assert_eq!(
            decoder.decode_next(),
            Err(DecodeError::PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
        );
    }

    #[test]
    fn decoder_rejects_unbounded_buffering() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.push(&vec![0; MAX_BUFFER_LEN + 1]),
            Err(DecodeError::BufferOverflow)
        );
    }

    #[test]
    fn paths_require_exact_single_nul_termination() {
        let mut payload = 5u16.to_ne_bytes().to_vec();
        payload.extend_from_slice(b"/dev\0");
        assert_eq!(
            parse_path(&payload).unwrap().as_os_str().as_bytes(),
            b"/dev"
        );

        let mut trailing = payload.clone();
        trailing.push(0);
        assert_eq!(parse_path(&trailing), Err(libc::EBADMSG));

        payload[3] = 0;
        assert_eq!(parse_path(&payload), Err(libc::EBADMSG));
    }

    #[test]
    fn paths_preserve_non_utf8_unix_bytes() {
        let path = b"/dev/input/\xff\0";
        let mut payload = (path.len() as u16).to_ne_bytes().to_vec();
        payload.extend_from_slice(path);
        assert_eq!(
            parse_path(&payload).unwrap().as_os_str().as_bytes(),
            b"/dev/input/\xff"
        );
    }

    #[test]
    fn fixed_size_messages_reject_trailing_data() {
        assert_eq!(parse_empty(&[0]), Err(libc::EBADMSG));
        assert_eq!(parse_i32(&[0; 5]), Err(libc::EBADMSG));
    }
}
