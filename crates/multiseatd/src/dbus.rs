use crate::acl::AclOps;
use crate::orchestrator::{OperationTimer, Orchestrator};
use dbus::arg::{ArgType, ReadAll};
use dbus::blocking::{Connection, stdintf::org_freedesktop_dbus::RequestNameReply};
use dbus::channel::{BusType, Channel};
use dbus::{Message, MessageType};
pub use libmultiseatd::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH};
use seat_udev::SeatName;
use seatd_server::ClientId;
use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::time::Duration;

pub const INTROSPECTION: &str =
    include_str!("../../../dist/dbus/org.decoupling.multiseatd1.Manager.xml");

pub struct Bus {
    connection: Connection,
}

impl Bus {
    pub fn new(session: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let mut channel = Channel::get_private(if session {
            BusType::Session
        } else {
            BusType::System
        })?;
        channel.set_watch_enabled(true);
        let connection = Connection::from(channel);
        if connection.request_name(BUS_NAME, false, false, true)? != RequestNameReply::PrimaryOwner
        {
            return Err(format!("{BUS_NAME} is already owned").into());
        }
        Ok(Self { connection })
    }

    pub fn watch(&self) -> dbus::channel::Watch {
        self.connection.channel().watch()
    }
    pub fn connected(&self) -> bool {
        self.connection.channel().is_connected()
    }
    pub fn has_output(&self) -> bool {
        self.connection.channel().has_messages_to_send()
    }

    /// Drain bounded batches on the main thread. A true return schedules
    /// another zero-timeout iteration, including messages buffered by UID calls.
    pub fn dispatch<A: AclOps>(&self, coordinator: &mut Orchestrator<A>) -> io::Result<bool> {
        self.connection
            .channel()
            .read_write(Some(Duration::ZERO))
            .map_err(|_| io::Error::other("lost the D-Bus connection"))?;
        for _ in 0..32 {
            let Some(message) = self.connection.channel().pop_message() else {
                return Ok(false);
            };
            if message.msg_type() != MessageType::MethodCall {
                continue;
            }
            let _timer = OperationTimer::new("D-Bus method");
            let reply = match self.caller_uid(&message) {
                Ok(0) => handle_method(coordinator, &message),
                _ => Err(("AccessDenied", "multiseatd is root-only".into())),
            };
            let reply = reply.unwrap_or_else(|(name, text)| error_reply(&message, name, &text));
            self.connection
                .channel()
                .send(reply)
                .map_err(|_| io::Error::other("D-Bus reply failed"))?;
        }
        Ok(true)
    }

    fn caller_uid(&self, message: &Message) -> Result<u32, dbus::Error> {
        let sender = message
            .sender()
            .ok_or_else(|| dbus::Error::new_failed("no sender"))?;
        let proxy = self.connection.with_proxy(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            Duration::from_secs(2),
        );
        let (uid,): (u32,) = proxy.method_call(
            "org.freedesktop.DBus",
            "GetConnectionUnixUser",
            (sender.to_string(),),
        )?;
        Ok(uid)
    }

    pub fn signal(&self, new: bool, seat: &SeatName) -> io::Result<()> {
        let signal = Message::new_signal(
            OBJECT_PATH,
            INTERFACE_NAME,
            if new { "SeatNew" } else { "SeatRemoved" },
        )
        .map_err(io::Error::other)?
        .append1(seat.as_str());
        self.connection
            .channel()
            .send(signal)
            .map_err(|_| io::Error::other("D-Bus signal failed"))?;
        Ok(())
    }
}

type MethodError = (&'static str, String);
fn arguments<T: ReadAll>(message: &Message) -> Result<T, MethodError> {
    let mut iter = message.iter_init();
    let args = T::read(&mut iter).map_err(|error| ("InvalidArgument", error.to_string()))?;
    if iter.arg_type() != ArgType::Invalid {
        return Err(("InvalidArgument", "unexpected trailing arguments".into()));
    }
    Ok(args)
}
fn backend(error: io::Error) -> MethodError {
    ("BackendFailure", error.to_string())
}

fn handle_method<A: AclOps>(
    coordinator: &mut Orchestrator<A>,
    message: &Message,
) -> Result<Message, MethodError> {
    if message.path().as_deref() != Some(OBJECT_PATH) {
        return Err(("InvalidArgument", "unknown object path".into()));
    }
    let interface = message
        .interface()
        .map(|value| value.to_string())
        .unwrap_or_default();
    let member = message
        .member()
        .map(|value| value.to_string())
        .unwrap_or_default();
    if interface == "org.freedesktop.DBus.Introspectable" && member == "Introspect" {
        arguments::<()>(message)?;
        return Ok(message.method_return().append1(INTROSPECTION));
    }
    if interface == "org.freedesktop.DBus.Peer" && member == "Ping" {
        arguments::<()>(message)?;
        return Ok(message.method_return());
    }
    if interface != INTERFACE_NAME {
        return Err(("InvalidArgument", "unknown interface".into()));
    }
    match member.as_str() {
        "ListSeats" => {
            arguments::<()>(message)?;
            Ok(message.method_return().append1(coordinator.list_seats()))
        }
        "ListClients" => {
            arguments::<()>(message)?;
            Ok(message.method_return().append1(coordinator.list_clients()))
        }
        "CreateClient" => {
            let (seat, uid): (String, u32) = arguments(message)?;
            let seat =
                SeatName::parse(seat).map_err(|error| ("InvalidArgument", error.to_string()))?;
            let (id, fd) = coordinator.create_client(&seat, uid).map_err(backend)?;
            // SAFETY: transfer the unique owned FD to libdbus's owned wrapper.
            let fd = unsafe { dbus::arg::OwnedFd::from_raw_fd(fd.into_raw_fd()) };
            Ok(message.method_return().append2(id, fd))
        }
        "RevokeClient" => {
            let (id,): (String,) = arguments(message)?;
            let uuid = uuid::Uuid::parse_str(&id)
                .map_err(|error| ("InvalidArgument", error.to_string()))?;
            if uuid.to_string() != id
                || uuid.get_version_num() != 7
                || uuid.get_variant() != uuid::Variant::RFC4122
            {
                return Err((
                    "InvalidArgument",
                    "client ID must be a canonical UUIDv7".into(),
                ));
            }
            coordinator
                .revoke_client(&ClientId::new(id))
                .map_err(backend)?;
            Ok(message.method_return())
        }
        _ => Err(("UnknownMethod", "unknown method".into())),
    }
}

fn error_reply(message: &Message, name: &str, text: &str) -> Message {
    let name = dbus::strings::ErrorName::new(format!("org.decoupling.multiseatd1.Error.{name}"))
        .expect("valid error name");
    let text = CString::new(text.replace('\0', "?")).expect("NUL removed");
    message.error(&name, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signatures_reject_trailing_and_mistyped_arguments() {
        let message =
            Message::new_method_call(BUS_NAME, OBJECT_PATH, INTERFACE_NAME, "CreateClient")
                .unwrap()
                .append2("seat1", 1000u32);
        assert_eq!(
            arguments::<(String, u32)>(&message).unwrap(),
            ("seat1".into(), 1000)
        );
        assert!(arguments::<(String,)>(&message).is_err());
        assert!(arguments::<(String, String)>(&message).is_err());
        assert!(INTROSPECTION.contains("a(ssus)"));
        assert!(!INTROSPECTION.contains("Refresh"));
    }
}
