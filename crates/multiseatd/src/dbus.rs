use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use zbus::object_server::SignalEmitter;

pub const BUS_NAME: &str = "org.decoupling.multiseatd1";
pub const OBJECT_PATH: &str = "/org/decoupling/multiseatd1";
pub const INTERFACE_NAME: &str = "org.decoupling.multiseatd1.Manager";

#[derive(Clone, Default)]
pub struct SeatRegistry {
    seats: Arc<RwLock<BTreeSet<String>>>,
}

impl SeatRegistry {
    pub fn insert(&self, seat: &str) {
        self.seats.write().unwrap().insert(seat.into());
    }

    pub fn remove(&self, seat: &str) {
        self.seats.write().unwrap().remove(seat);
    }

    pub fn list(&self) -> Vec<String> {
        self.seats.read().unwrap().iter().cloned().collect()
    }
}

pub struct Manager {
    registry: SeatRegistry,
}

impl Manager {
    pub fn new(registry: SeatRegistry) -> Self {
        Self { registry }
    }
}

#[zbus::interface(name = "org.decoupling.multiseatd1.Manager")]
impl Manager {
    async fn list_seats(&self) -> Vec<String> {
        self.registry.list()
    }

    #[zbus(signal)]
    pub async fn seat_new(emitter: &SignalEmitter<'_>, seat: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn seat_removed(emitter: &SignalEmitter<'_>, seat: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_sorted_and_idempotent() {
        let registry = SeatRegistry::default();
        registry.insert("seat2");
        registry.insert("seat0");
        registry.insert("seat0");
        assert_eq!(registry.list(), ["seat0", "seat2"]);
        registry.remove("seat0");
        assert_eq!(registry.list(), ["seat2"]);
        assert_eq!(INTERFACE_NAME, "org.decoupling.multiseatd1.Manager");
    }
}
