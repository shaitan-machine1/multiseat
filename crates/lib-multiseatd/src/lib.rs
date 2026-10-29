//! Synchronous public D-Bus client and protocol types for multiseatd.
use dbus::blocking::{Connection, Proxy};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::time::Duration;

pub const BUS_NAME: &str = "org.decoupling.multiseatd1";
pub const OBJECT_PATH: &str = "/org/decoupling/multiseatd1";
pub const INTERFACE_NAME: &str = "org.decoupling.multiseatd1.Manager";
pub type ClientRow = (String, String, u32, String);
pub const INTROSPECTION: &str =
    include_str!("../../../dist/dbus/org.decoupling.multiseatd1.Manager.xml");

pub struct Client {
    connection: Connection,
    timeout: Duration,
}
impl Client {
    pub fn system(timeout: Duration) -> Result<Self, dbus::Error> {
        Ok(Self {
            connection: Connection::new_system()?,
            timeout,
        })
    }
    pub fn session(timeout: Duration) -> Result<Self, dbus::Error> {
        Ok(Self {
            connection: Connection::new_session()?,
            timeout,
        })
    }
    fn proxy(&self) -> Proxy<'static, &Connection> {
        self.connection
            .with_proxy(BUS_NAME, OBJECT_PATH, self.timeout)
    }
    pub fn list_seats(&self) -> Result<Vec<String>, dbus::Error> {
        let (seats,): (Vec<String>,) = self.proxy().method_call(INTERFACE_NAME, "ListSeats", ())?;
        Ok(seats)
    }
    /// Create a seatd connection. The returned descriptor is owned by the caller.
    pub fn create_client(&self, seat: &str, uid: u32) -> Result<(String, OwnedFd), dbus::Error> {
        let (id, fd): (String, dbus::arg::OwnedFd) =
            self.proxy()
                .method_call(INTERFACE_NAME, "CreateClient", (seat, uid))?;
        // SAFETY: ownership is transferred out of the D-Bus descriptor exactly once.
        let received = unsafe { OwnedFd::from_raw_fd(fd.into_raw_fd()) };
        // libdbus may duplicate descriptors without CLOEXEC. Return a separate
        // close-on-exec duplicate so the lease cannot leak into child processes.
        let fd = received
            .try_clone()
            .map_err(|error| dbus::Error::new_failed(&format!("duplicate seatd fd: {error}")))?;
        Ok((id, fd))
    }

    /// Revoke a client; revoking an already-removed client is successful.
    pub fn revoke_client(&self, client_id: &str) -> Result<(), dbus::Error> {
        self.proxy()
            .method_call(INTERFACE_NAME, "RevokeClient", (client_id,))
    }

    pub fn list_clients(&self) -> Result<Vec<ClientRow>, dbus::Error> {
        let (clients,): (Vec<ClientRow>,) =
            self.proxy()
                .method_call(INTERFACE_NAME, "ListClients", ())?;
        Ok(clients)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_names_are_stable() {
        assert_eq!(BUS_NAME, "org.decoupling.multiseatd1");
        assert_eq!(OBJECT_PATH, "/org/decoupling/multiseatd1");
        assert_eq!(INTERFACE_NAME, "org.decoupling.multiseatd1.Manager");
        use dbus::arg::Arg;
        assert_eq!(<Vec<ClientRow>>::signature().to_string(), "a(ssus)");
        assert_eq!(dbus::arg::OwnedFd::signature().to_string(), "h");
        assert!(INTROSPECTION.contains("CreateClient"));
        assert!(!INTROSPECTION.contains("Refresh"));
    }
}
