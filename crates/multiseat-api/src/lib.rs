//! Typed public D-Bus protocol for multiseatd.

pub const BUS_NAME: &str = "org.decoupling.multiseatd1";
pub const OBJECT_PATH: &str = "/org/decoupling/multiseatd1";
pub const INTERFACE_NAME: &str = "org.decoupling.multiseatd1.Manager";

pub type RefreshDescription = (String, String, Vec<String>, Vec<String>);

#[zbus::proxy(
    interface = "org.decoupling.multiseatd1.Manager",
    default_service = "org.decoupling.multiseatd1",
    default_path = "/org/decoupling/multiseatd1"
)]
pub trait Manager {
    fn list_seats(&self) -> zbus::Result<Vec<String>>;
    fn preview_refresh(&self, seat: &str) -> zbus::Result<Vec<RefreshDescription>>;
    fn refresh(&self, seat: &str, confirmed: bool) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn seat_new(&self, seat: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn seat_removed(&self, seat: &str) -> zbus::Result<()>;
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
