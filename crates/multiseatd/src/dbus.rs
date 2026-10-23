use crate::orchestrator::{Orchestrator, RefreshChange};
pub use multiseat_api::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH, RefreshDescription};
use seat_udev::SeatName;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use zbus::Connection;
use zbus::fdo;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;

#[derive(Clone, Default)]
pub struct SeatRegistry {
    seats: Arc<RwLock<BTreeSet<String>>>,
}

impl SeatRegistry {
    pub fn insert(&self, seat: &SeatName) -> bool {
        self.seats.write().unwrap().insert(seat.to_string())
    }

    pub fn remove(&self, seat: &SeatName) -> bool {
        self.seats.write().unwrap().remove(seat.as_str())
    }

    pub fn list(&self) -> Vec<String> {
        self.seats.read().unwrap().iter().cloned().collect()
    }
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.decoupling.multiseatd1.Error")]
pub enum ManagerError {
    AccessDenied(String),
    InvalidArgument(String),
    NotConfirmed(String),
    BackendFailure(String),
}

pub struct Manager {
    registry: SeatRegistry,
    orchestrator: Arc<Mutex<Orchestrator>>,
}

impl Manager {
    pub fn new(registry: SeatRegistry, orchestrator: Arc<Mutex<Orchestrator>>) -> Self {
        Self {
            registry,
            orchestrator,
        }
    }
}

#[zbus::interface(name = "org.decoupling.multiseatd1.Manager")]
impl Manager {
    async fn list_seats(&self) -> Vec<String> {
        self.registry.list()
    }

    async fn preview_refresh(&self, seat: String) -> Result<Vec<RefreshDescription>, ManagerError> {
        let selected = parse_optional_seat(&seat)?;
        let orchestrator = self.orchestrator.lock().await;
        let changes = orchestrator
            .plan(selected.as_ref())
            .map_err(backend_error)?;
        Ok(changes.iter().map(describe_change).collect())
    }

    async fn refresh(
        &self,
        seat: String,
        confirmed: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<Vec<String>, ManagerError> {
        require_root(caller_uid(connection, &header).await?)?;
        if !confirmed {
            return Err(ManagerError::NotConfirmed(
                "refresh is disruptive; explicit confirmation is required".into(),
            ));
        }
        let selected = parse_optional_seat(&seat)?;
        let orchestrator = self.orchestrator.lock().await;
        let changes = orchestrator
            .plan(selected.as_ref())
            .map_err(backend_error)?;
        let mut completed = Vec::new();
        for change in changes {
            if change.deployed.is_some() && self.registry.remove(&change.seat) {
                Self::seat_removed(&emitter, change.seat.as_str())
                    .await
                    .map_err(|error| ManagerError::BackendFailure(error.to_string()))?;
            }
            orchestrator.apply(&change).map_err(backend_error)?;
            if change.desired.is_some() {
                self.registry.insert(&change.seat);
                Self::seat_new(&emitter, change.seat.as_str())
                    .await
                    .map_err(|error| ManagerError::BackendFailure(error.to_string()))?;
            }
            completed.push(change.seat.to_string());
        }
        Ok(completed)
    }

    #[zbus(signal)]
    pub async fn seat_new(emitter: &SignalEmitter<'_>, seat: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn seat_removed(emitter: &SignalEmitter<'_>, seat: &str) -> zbus::Result<()>;
}

fn parse_optional_seat(value: &str) -> Result<Option<SeatName>, ManagerError> {
    if value.is_empty() {
        Ok(None)
    } else {
        SeatName::parse(value)
            .map(Some)
            .map_err(|error| ManagerError::InvalidArgument(error.to_string()))
    }
}

fn describe_change(change: &RefreshChange) -> RefreshDescription {
    let old = change
        .deployed
        .as_ref()
        .map(|config| {
            config
                .devices
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let new = change
        .desired
        .as_ref()
        .map(|config| {
            config
                .devices
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    (
        change.seat.to_string(),
        change.kind.as_str().into(),
        old,
        new,
    )
}

fn backend_error(error: impl std::fmt::Display) -> ManagerError {
    ManagerError::BackendFailure(error.to_string())
}

async fn caller_uid(
    connection: &Connection,
    header: &Header<'_>,
) -> Result<Option<u32>, ManagerError> {
    let sender = header
        .sender()
        .ok_or_else(|| ManagerError::AccessDenied("message has no sender".into()))?;
    let proxy = fdo::DBusProxy::new(connection)
        .await
        .map_err(backend_error)?;
    proxy
        .get_connection_credentials(BusName::from(sender.clone()))
        .await
        .map(|credentials| credentials.unix_user_id())
        .map_err(backend_error)
}

fn require_root(uid: Option<u32>) -> Result<(), ManagerError> {
    if uid == Some(0) {
        Ok(())
    } else {
        Err(ManagerError::AccessDenied(
            "seat refresh is root-only".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_sorted_and_idempotent() {
        let registry = SeatRegistry::default();
        let seat2 = SeatName::parse("seat2").unwrap();
        let seat0 = SeatName::seat0();
        registry.insert(&seat2);
        registry.insert(&seat0);
        registry.insert(&seat0);
        assert_eq!(registry.list(), ["seat0", "seat2"]);
        registry.remove(&seat0);
        assert_eq!(registry.list(), ["seat2"]);
        assert_eq!(INTERFACE_NAME, "org.decoupling.multiseatd1.Manager");
    }
}
