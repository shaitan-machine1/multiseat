use std::io;

pub const MAX_PATH_LEN: usize = 256;
pub const MAX_SEAT_LEN: usize = 64;

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

#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub fn decode_next(&mut self) -> Option<Packet> {
        if self.bytes.len() < 4 {
            return None;
        }
        let opcode = u16::from_ne_bytes([self.bytes[0], self.bytes[1]]);
        let size = u16::from_ne_bytes([self.bytes[2], self.bytes[3]]) as usize;
        if self.bytes.len() < 4 + size {
            return None;
        }
        let payload = self.bytes[4..4 + size].to_vec();
        self.bytes.drain(..4 + size);
        Some(Packet { opcode, payload })
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

pub fn parse_path(payload: &[u8]) -> Result<&str, i32> {
    if payload.len() < 2 {
        return Err(libc::EBADMSG);
    }
    let len = u16::from_ne_bytes([payload[0], payload[1]]) as usize;
    if len == 0 || len > MAX_PATH_LEN || payload.len() != 2 + len || payload[2 + len - 1] != 0 {
        return Err(libc::EBADMSG);
    }
    std::str::from_utf8(&payload[2..2 + len - 1]).map_err(|_| libc::EBADMSG)
}

pub fn parse_i32(payload: &[u8]) -> Result<i32, i32> {
    if payload.len() != 4 {
        return Err(libc::EBADMSG);
    }
    Ok(i32::from_ne_bytes(payload.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_fragmented_and_coalesced_native_wire_packets() {
        let mut decoder = Decoder::default();
        let mut bytes = empty(CLIENT_OPEN_SEAT);
        bytes.extend(empty(CLIENT_PING));
        decoder.push(&bytes[..3]);
        assert!(decoder.decode_next().is_none());
        decoder.push(&bytes[3..]);
        assert_eq!(decoder.decode_next().unwrap().opcode, CLIENT_OPEN_SEAT);
        assert_eq!(decoder.decode_next().unwrap().opcode, CLIENT_PING);
        assert!(decoder.decode_next().is_none());
    }

    #[test]
    fn paths_require_exact_nul_terminated_payloads() {
        let mut payload = 5u16.to_ne_bytes().to_vec();
        payload.extend_from_slice(b"/dev\0");
        assert_eq!(parse_path(&payload).unwrap(), "/dev");
        payload.pop();
        assert_eq!(parse_path(&payload), Err(libc::EBADMSG));
    }
}
