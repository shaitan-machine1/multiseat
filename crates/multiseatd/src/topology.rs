use crate::access::DirectNodes;
use crate::brokered::authorization;
use seat_udev::{Context, SeatDevice, SeatName};
use seatd_server::AllowedDevice;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub seats: BTreeSet<SeatName>,
    pub brokered: BTreeMap<SeatName, Vec<AllowedDevice>>,
    pub direct: DirectNodes,
}

impl Snapshot {
    pub fn from_context(context: &Context, test_seats: &[SeatName]) -> io::Result<Self> {
        let seats = context
            .seats()
            .into_iter()
            .chain(test_seats.iter().cloned())
            .collect();
        let static_nodes = context
            .static_direct_access_nodes(&SeatName::seat0())
            .map_err(io::Error::other)?;
        Self::from_devices(seats, context.all_devices(), static_nodes)
    }

    pub fn from_devices(
        seats: BTreeSet<SeatName>,
        devices: Vec<SeatDevice>,
        static_nodes: Vec<PathBuf>,
    ) -> io::Result<Self> {
        let mut snapshot = Self {
            seats,
            ..Self::default()
        };
        let mut brokered_paths = BTreeSet::new();
        let brokered_numbers = devices
            .iter()
            .filter(|device| device.brokerable())
            .filter_map(|device| device.devnum)
            .collect::<BTreeSet<_>>();
        let mut known_paths = BTreeSet::new();
        let known_numbers = devices
            .iter()
            .filter_map(|device| device.devnum)
            .collect::<BTreeSet<_>>();
        for device in &devices {
            if let Some(path) = &device.devnode {
                let path = match std::fs::canonicalize(path) {
                    Ok(path) => path,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                known_paths.insert(path.clone());
                if device.brokerable() {
                    brokered_paths.insert(path);
                }
            }
        }
        for device in devices {
            if device.brokerable() && snapshot.seats.contains(&device.seat) {
                match authorization(&device) {
                    Ok(Some(allowed)) => snapshot
                        .brokered
                        .entry(device.seat)
                        .or_default()
                        .push(allowed),
                    Ok(None) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            } else if device.direct_access()
                && !device
                    .devnum
                    .is_some_and(|number| brokered_numbers.contains(&number))
            {
                let path = device.devnode.expect("direct device has a node");
                match std::fs::canonicalize(path) {
                    Ok(path) if !brokered_paths.contains(&path) => {
                        snapshot.direct.entry(device.seat).or_default().insert(path);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        for path in static_nodes {
            let path = match std::fs::canonicalize(path) {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&path)?;
            let kind = metadata.file_type();
            if (kind.is_char_device() || kind.is_block_device())
                && !known_paths.contains(&path)
                && !known_numbers.contains(&metadata.rdev())
            {
                snapshot
                    .direct
                    .entry(SeatName::seat0())
                    .or_default()
                    .insert(path);
            }
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seat_udev::DeviceKind;
    use std::os::unix::fs::MetadataExt;

    fn device(path: &str, kind: DeviceKind, seat: &str) -> SeatDevice {
        SeatDevice {
            syspath: format!("/sys{path}").into(),
            devnode: Some(path.into()),
            devnum: Some(std::fs::metadata(path).unwrap().rdev()),
            seat: SeatName::parse(seat).unwrap(),
            kind,
            uaccess: true,
            master_of_seat: false,
        }
    }

    #[test]
    fn brokered_devices_never_receive_direct_or_static_authorization() {
        let seat = SeatName::parse("seat-test").unwrap();
        let brokered = device("/dev/null", DeviceKind::Evdev, seat.as_str());
        let direct = device("/dev/zero", DeviceKind::Other, seat.as_str());
        let snapshot = Snapshot::from_devices(
            BTreeSet::from([seat.clone()]),
            vec![brokered, direct],
            vec!["/dev/null".into(), "/dev/zero".into()],
        )
        .unwrap();
        assert_eq!(snapshot.brokered[&seat].len(), 1);
        assert_eq!(
            snapshot.direct[&seat],
            BTreeSet::from([PathBuf::from("/dev/zero")])
        );
        assert!(!snapshot.direct.contains_key(&SeatName::seat0()));
    }

    #[test]
    fn startup_clears_direct_devices_on_unpublished_seats_and_includes_static_seat0_nodes() {
        let absent = SeatName::parse("seat-absent").unwrap();
        let snapshot = Snapshot::from_devices(
            BTreeSet::new(),
            vec![device("/dev/zero", DeviceKind::Other, absent.as_str())],
            vec!["/dev/null".into()],
        )
        .unwrap();
        assert!(snapshot.seats.is_empty());
        assert!(snapshot.brokered.is_empty());
        assert_eq!(
            snapshot.direct[&absent],
            BTreeSet::from([PathBuf::from("/dev/zero")])
        );
        assert_eq!(
            snapshot.direct[&SeatName::seat0()],
            BTreeSet::from([PathBuf::from("/dev/null")])
        );
    }

    #[test]
    fn same_seat_reclassification_moves_between_exclusive_resource_classes() {
        let seat = SeatName::parse("seat-test").unwrap();
        let mut source = device("/dev/null", DeviceKind::Evdev, seat.as_str());
        let before = Snapshot::from_devices(
            BTreeSet::from([seat.clone()]),
            vec![source.clone()],
            Vec::new(),
        )
        .unwrap();
        source.kind = DeviceKind::Other;
        let after =
            Snapshot::from_devices(BTreeSet::from([seat.clone()]), vec![source], Vec::new())
                .unwrap();
        assert!(before.direct.is_empty());
        assert!(after.brokered.is_empty());
        assert_eq!(
            after.direct[&seat],
            BTreeSet::from([PathBuf::from("/dev/null")])
        );
    }
}
