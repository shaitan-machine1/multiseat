//! Synchronous public D-Bus client and protocol types for multiseatd.
use dbus::blocking::{Connection, Proxy};
use std::time::Duration;

pub const BUS_NAME: &str = "org.decoupling.multiseatd1";
pub const OBJECT_PATH: &str = "/org/decoupling/multiseatd1";
pub const INTERFACE_NAME: &str = "org.decoupling.multiseatd1.Manager";
pub type RefreshDescription = (String, String, Vec<String>, Vec<String>);

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
    pub fn preview_refresh(&self, seat: &str) -> Result<Vec<RefreshDescription>, dbus::Error> {
        let (changes,): (Vec<RefreshDescription>,) =
            self.proxy()
                .method_call(INTERFACE_NAME, "PreviewRefresh", (seat,))?;
        Ok(changes)
    }
    pub fn refresh(&self, seat: &str, confirmed: bool) -> Result<Vec<String>, dbus::Error> {
        let (seats,): (Vec<String>,) =
            self.proxy()
                .method_call(INTERFACE_NAME, "Refresh", (seat, confirmed))?;
        Ok(seats)
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
    }
}
