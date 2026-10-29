//! Coordinator-side brokered authorization reconciliation.
//!
//! All source-seat removals are acknowledged before any destination-seat
//! additions. Retained inventory is deliberately conservative after failures:
//! an unsuccessful removal is retried, never interpreted as a completed move.
use seat_udev::{SeatDevice, SeatName};
use seatd_server::{AllowedDevice, DeviceId, DeviceKind, SeatHandle};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::ffi::OsStrExt;

pub fn authorization(device: &SeatDevice) -> io::Result<Option<AllowedDevice>> {
    if !device.brokerable() {
        return Ok(None);
    }
    let kind = match device.kind {
        seat_udev::DeviceKind::Drm => DeviceKind::Drm,
        seat_udev::DeviceKind::Evdev => DeviceKind::Evdev,
        seat_udev::DeviceKind::Hidraw => DeviceKind::Hidraw,
        seat_udev::DeviceKind::Other => return Ok(None),
    };
    let number = device.devnum.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "brokered device has no device number",
        )
    })?;
    AllowedDevice::canonicalized(
        // Preserve non-UTF-8 topology IDs without lossy string collisions.
        DeviceId::new(hex::encode(device.syspath.as_os_str().as_bytes())),
        device
            .devnode
            .as_ref()
            .expect("brokerable device has a devnode"),
        number,
        kind,
    )
    .map(Some)
}

/// Private daemon test seam; production uses synchronous seat actor commands.
pub trait DeviceCommands {
    fn add(&self, device: AllowedDevice) -> io::Result<()>;
    fn remove(&self, id: DeviceId) -> io::Result<()>;
}

impl DeviceCommands for SeatHandle {
    fn add(&self, device: AllowedDevice) -> io::Result<()> {
        self.add_device(device)
    }

    fn remove(&self, id: DeviceId) -> io::Result<()> {
        self.remove_device(id)
    }
}

#[derive(Default)]
pub struct BrokeredDevices {
    applied: BTreeMap<SeatName, BTreeMap<DeviceId, AllowedDevice>>,
    pending_removals: BTreeSet<(SeatName, DeviceId)>,
}

impl BrokeredDevices {
    pub fn reconcile<S: DeviceCommands>(
        &mut self,
        seats: &BTreeMap<SeatName, S>,
        desired: &BTreeMap<SeatName, Vec<AllowedDevice>>,
    ) -> io::Result<()> {
        // Reject ambiguous topology before performing any device operation.
        let mut wanted = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for (seat, devices) in desired {
            if !seats.contains_key(seat) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no actor for {seat}"),
                ));
            }
            let mut entries = BTreeMap::new();
            for device in devices {
                let mut device = device.clone();
                device.path = std::fs::canonicalize(&device.path)?;
                if !ids.insert(device.id.clone()) || !paths.insert(device.path.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "duplicate brokered identity or canonical path",
                    ));
                }
                entries.insert(device.id.clone(), device);
            }
            wanted.insert(seat.clone(), entries);
        }

        let mut first_error = None;
        for (seat, devices) in &mut self.applied {
            let removed = devices
                .iter()
                .filter(|(id, old)| {
                    self.pending_removals
                        .contains(&(seat.clone(), (*id).clone()))
                        || wanted.get(seat).and_then(|entries| entries.get(*id)) != Some(*old)
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in removed {
                let result = seats
                    .get(seat)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("missing source actor for {seat}"),
                        )
                    })
                    .and_then(|actor| actor.remove(id.clone()));
                match result {
                    Ok(()) => {
                        devices.remove(&id);
                        self.pending_removals.remove(&(seat.clone(), id));
                    }
                    Err(error) => {
                        self.pending_removals.insert((seat.clone(), id));
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        // No additions anywhere if any old privileged use could not be revoked.
        if let Some(error) = first_error {
            return Err(error);
        }
        self.applied.retain(|_, devices| !devices.is_empty());

        for (seat, devices) in wanted {
            for (id, device) in devices {
                if self.applied.get(&seat).and_then(|entries| entries.get(&id)) == Some(&device) {
                    continue;
                }
                seats[&seat].add(device.clone())?;
                self.applied
                    .entry(seat.clone())
                    .or_default()
                    .insert(id, device);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeSeat {
        name: String,
        calls: Arc<Mutex<Vec<String>>>,
        fail_remove: Arc<Mutex<bool>>,
        fail_add: Arc<Mutex<bool>>,
    }

    impl DeviceCommands for FakeSeat {
        fn add(&self, device: AllowedDevice) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("add:{}:{}", self.name, device.id));
            if *self.fail_add.lock().unwrap() {
                Err(io::Error::other("add failed"))
            } else {
                Ok(())
            }
        }

        fn remove(&self, id: DeviceId) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("remove:{}:{id}", self.name));
            if *self.fail_remove.lock().unwrap() {
                Err(io::Error::other("remove failed"))
            } else {
                Ok(())
            }
        }
    }

    fn seat(name: &str) -> SeatName {
        SeatName::parse(name).unwrap()
    }

    fn actors() -> BTreeMap<SeatName, FakeSeat> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        ["seat1", "seat9"]
            .into_iter()
            .map(|name| {
                (
                    seat(name),
                    FakeSeat {
                        name: name.into(),
                        calls: calls.clone(),
                        fail_remove: Arc::default(),
                        fail_add: Arc::default(),
                    },
                )
            })
            .collect()
    }

    fn device(path: &str) -> AllowedDevice {
        AllowedDevice::canonicalized(
            DeviceId::new(path),
            path,
            std::fs::metadata(path).unwrap().rdev(),
            DeviceKind::Evdev,
        )
        .unwrap()
    }

    #[test]
    fn moves_and_swaps_remove_every_source_before_adding_any_destination() {
        let actors = actors();
        let mut inventory = BrokeredDevices::default();
        let initial = BTreeMap::from([
            (seat("seat1"), vec![device("/dev/zero")]),
            (seat("seat9"), vec![device("/dev/null")]),
        ]);
        inventory.reconcile(&actors, &initial).unwrap();
        let calls = &actors[&seat("seat1")].calls;
        calls.lock().unwrap().clear();
        let moved = BTreeMap::from([
            (seat("seat1"), vec![device("/dev/null")]),
            (seat("seat9"), vec![device("/dev/zero")]),
        ]);
        inventory.reconcile(&actors, &moved).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "remove:seat1:/dev/zero",
                "remove:seat9:/dev/null",
                "add:seat1:/dev/null",
                "add:seat9:/dev/zero",
            ]
        );
        calls.lock().unwrap().clear();
        inventory.reconcile(&actors, &moved).unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_source_removal_blocks_destination_and_is_retried() {
        let actors = actors();
        let mut inventory = BrokeredDevices::default();
        inventory
            .reconcile(
                &actors,
                &BTreeMap::from([(seat("seat9"), vec![device("/dev/null")])]),
            )
            .unwrap();
        let source = &actors[&seat("seat9")];
        source.calls.lock().unwrap().clear();
        *source.fail_remove.lock().unwrap() = true;
        let moved = BTreeMap::from([(seat("seat1"), vec![device("/dev/null")])]);
        assert!(inventory.reconcile(&actors, &moved).is_err());
        assert_eq!(*source.calls.lock().unwrap(), ["remove:seat9:/dev/null"]);
        *source.fail_remove.lock().unwrap() = false;
        inventory.reconcile(&actors, &moved).unwrap();
        assert_eq!(
            *source.calls.lock().unwrap(),
            [
                "remove:seat9:/dev/null",
                "remove:seat9:/dev/null",
                "add:seat1:/dev/null",
            ]
        );
    }

    #[test]
    fn failed_move_is_finished_even_when_topology_returns_to_the_source() {
        let actors = actors();
        let source = &actors[&seat("seat9")];
        let mut inventory = BrokeredDevices::default();
        let initial = BTreeMap::from([(seat("seat9"), vec![device("/dev/null")])]);
        inventory.reconcile(&actors, &initial).unwrap();
        source.calls.lock().unwrap().clear();
        *source.fail_remove.lock().unwrap() = true;
        assert!(inventory.reconcile(&actors, &BTreeMap::new()).is_err());
        *source.fail_remove.lock().unwrap() = false;
        inventory.reconcile(&actors, &initial).unwrap();
        assert_eq!(
            *source.calls.lock().unwrap(),
            [
                "remove:seat9:/dev/null",
                "remove:seat9:/dev/null",
                "add:seat9:/dev/null",
            ]
        );
        assert!(inventory.pending_removals.is_empty());
    }

    #[test]
    fn failed_addition_is_not_recorded_as_applied() {
        let actors = actors();
        let target = &actors[&seat("seat1")];
        *target.fail_add.lock().unwrap() = true;
        let mut inventory = BrokeredDevices::default();
        let desired = BTreeMap::from([(seat("seat1"), vec![device("/dev/null")])]);
        assert!(inventory.reconcile(&actors, &desired).is_err());
        assert!(inventory.applied.is_empty());
        *target.fail_add.lock().unwrap() = false;
        inventory.reconcile(&actors, &desired).unwrap();
        assert_eq!(target.calls.lock().unwrap().len(), 2);
        inventory.reconcile(&actors, &BTreeMap::new()).unwrap();
        assert!(inventory.applied.is_empty());
    }

    #[test]
    fn ambiguous_or_unready_topology_is_rejected_without_commands() {
        let actors = actors();
        let mut inventory = BrokeredDevices::default();
        let duplicate = BTreeMap::from([
            (seat("seat1"), vec![device("/dev/null")]),
            (seat("seat9"), vec![device("/dev/null")]),
        ]);
        assert!(inventory.reconcile(&actors, &duplicate).is_err());
        let missing = BTreeMap::from([(seat("seat2"), vec![device("/dev/null")])]);
        assert!(inventory.reconcile(&actors, &missing).is_err());
        assert!(actors[&seat("seat1")].calls.lock().unwrap().is_empty());
    }

    #[test]
    fn topology_mapping_preserves_identity_and_excludes_direct_devices() {
        let mut source = SeatDevice {
            syspath: PathBuf::from("/sys/devices/input0"),
            devnode: Some("/dev/null".into()),
            devnum: Some(std::fs::metadata("/dev/null").unwrap().rdev()),
            seat: seat("seat1"),
            kind: seat_udev::DeviceKind::Evdev,
            uaccess: true,
            master_of_seat: false,
        };
        let first = authorization(&source).unwrap().unwrap();
        source.seat = seat("seat9");
        source.uaccess = false;
        assert_eq!(authorization(&source).unwrap().unwrap(), first);
        source.kind = seat_udev::DeviceKind::Other;
        source.uaccess = true;
        assert!(authorization(&source).unwrap().is_none());
        source.kind = seat_udev::DeviceKind::Hidraw;
        assert_eq!(
            authorization(&source).unwrap().unwrap().kind,
            DeviceKind::Hidraw
        );
        source.devnum = None;
        assert!(authorization(&source).is_err());
    }
}
