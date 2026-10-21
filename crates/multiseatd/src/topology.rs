use crate::config::{ConfigError, SeatConfig};
use seat_udev::{Context, Filter, SeatName};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TopologyError {
    #[error("could not enumerate udev: {0}")]
    Udev(#[from] seat_udev::Error),
    #[error("could not canonicalize device {path:?}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Config(#[from] ConfigError),
}

pub trait Topology: Send + Sync {
    fn snapshot(&self) -> Result<BTreeMap<SeatName, SeatConfig>, TopologyError>;
}

pub struct UdevTopology {
    runtime_dir: PathBuf,
    test_seats: Vec<SeatName>,
}

impl UdevTopology {
    pub fn new(runtime_dir: PathBuf, test_seats: Vec<SeatName>) -> Self {
        Self {
            runtime_dir,
            test_seats,
        }
    }
}

impl Topology for UdevTopology {
    fn snapshot(&self) -> Result<BTreeMap<SeatName, SeatConfig>, TopologyError> {
        // Context creates a monitor before enumerating to provide a coherent
        // snapshot. It is dropped immediately: multiseatd intentionally does
        // not continuously consume udev events.
        let context = Context::new()?;
        let mut result = BTreeMap::new();
        for seat in context
            .seats()
            .into_iter()
            .chain(self.test_seats.iter().cloned())
        {
            if result.contains_key(&seat) {
                continue;
            }
            let mut devices = Vec::new();
            for device in context.devices(&seat, Filter::Brokerable) {
                if let Some(path) = device.devnode {
                    let canonical =
                        fs::canonicalize(&path).map_err(|source| TopologyError::Canonicalize {
                            path: path.clone(),
                            source,
                        })?;
                    devices.push(canonical);
                }
            }
            let vt_bound = seat == SeatName::seat0();
            let config = SeatConfig::new(seat.clone(), &self.runtime_dir, vt_bound, devices)?;
            result.insert(seat, config);
        }
        Ok(result)
    }
}
