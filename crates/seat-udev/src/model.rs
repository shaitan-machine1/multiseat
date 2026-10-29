use crate::SeatName;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeviceKind {
    Drm,
    Evdev,
    Hidraw,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SeatDevice {
    pub syspath: PathBuf,
    pub devnode: Option<PathBuf>,
    pub devnum: Option<u64>,
    pub seat: SeatName,
    pub kind: DeviceKind,
    pub uaccess: bool,
    pub master_of_seat: bool,
}

impl SeatDevice {
    pub fn brokerable(&self) -> bool {
        matches!(
            self.kind,
            DeviceKind::Drm | DeviceKind::Evdev | DeviceKind::Hidraw
        ) && self.devnode.is_some()
    }

    pub fn direct_access(&self) -> bool {
        self.uaccess && !self.brokerable() && self.devnode.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filter {
    All,
    Brokerable,
    DirectAccess,
    Uaccess,
}

impl Filter {
    fn matches(self, device: &SeatDevice) -> bool {
        match self {
            Self::All => true,
            Self::Brokerable => device.brokerable(),
            Self::DirectAccess => device.direct_access(),
            Self::Uaccess => device.uaccess,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    DeviceAdded(SeatDevice),
    DeviceRemoved(SeatDevice),
    DeviceMoved { old: SeatDevice, new: SeatDevice },
    DeviceChanged { old: SeatDevice, new: SeatDevice },
    SeatAdded(SeatName),
    SeatRemoved(SeatName),
}

/// Pure cache and transition logic. The real libudev context feeds this model;
/// tests and consumers can exercise it without libudev, root, or hardware.
#[derive(Clone, Debug, Default)]
pub struct Model {
    devices: BTreeMap<PathBuf, SeatDevice>,
}

impl Model {
    pub fn seed(&mut self, device: SeatDevice) {
        self.devices.insert(device.syspath.clone(), device);
    }

    pub fn add_or_change(&mut self, device: SeatDevice) -> Vec<Event> {
        let before = self.seats();
        let old = self.devices.insert(device.syspath.clone(), device.clone());
        let after = self.seats();
        let mut events = Vec::new();

        match old {
            None => {
                push_added_seats(&before, &after, &mut events);
                events.push(Event::DeviceAdded(device));
            }
            Some(old) if old == device => {}
            Some(old) if old.seat != device.seat => {
                events.push(Event::DeviceMoved { old, new: device });
                push_removed_seats(&before, &after, &mut events);
                push_added_seats(&before, &after, &mut events);
            }
            Some(old) => {
                push_added_seats(&before, &after, &mut events);
                events.push(Event::DeviceChanged { old, new: device });
                push_removed_seats(&before, &after, &mut events);
            }
        }
        events
    }

    pub fn remove(&mut self, syspath: &Path) -> Vec<Event> {
        let before = self.seats();
        let Some(old) = self.devices.remove(syspath) else {
            return Vec::new();
        };
        let after = self.seats();
        let mut events = vec![Event::DeviceRemoved(old)];
        push_removed_seats(&before, &after, &mut events);
        events
    }

    pub fn seats(&self) -> BTreeSet<SeatName> {
        self.devices
            .values()
            .filter(|device| device.master_of_seat)
            .map(|device| device.seat.clone())
            .collect()
    }

    pub fn devices(&self, seat: &SeatName, filter: Filter) -> Vec<SeatDevice> {
        self.devices
            .values()
            .filter(|device| &device.seat == seat && filter.matches(device))
            .cloned()
            .collect()
    }

    pub fn all_devices(&self) -> Vec<SeatDevice> {
        self.devices.values().cloned().collect()
    }

    pub fn brokered_devices(&self, seat: &SeatName) -> Vec<SeatDevice> {
        self.devices(seat, Filter::Brokerable)
    }

    pub fn direct_access_devices(&self, seat: &SeatName) -> Vec<SeatDevice> {
        self.devices(seat, Filter::DirectAccess)
    }

    pub fn by_syspath(&self, syspath: &Path) -> Option<SeatDevice> {
        self.devices.get(syspath).cloned()
    }

    pub fn by_devnode(&self, devnode: &Path) -> Option<SeatDevice> {
        self.devices
            .values()
            .find(|device| device.devnode.as_deref() == Some(devnode))
            .cloned()
    }

    pub fn by_devnum(&self, devnum: u64) -> Option<SeatDevice> {
        self.devices
            .values()
            .find(|device| device.devnum == Some(devnum))
            .cloned()
    }
}

fn push_added_seats(
    before: &BTreeSet<SeatName>,
    after: &BTreeSet<SeatName>,
    events: &mut Vec<Event>,
) {
    events.extend(after.difference(before).cloned().map(Event::SeatAdded));
}

fn push_removed_seats(
    before: &BTreeSet<SeatName>,
    after: &BTreeSet<SeatName>,
    events: &mut Vec<Event>,
) {
    events.extend(before.difference(after).cloned().map(Event::SeatRemoved));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &str, seat: &str, master: bool) -> SeatDevice {
        SeatDevice {
            syspath: path.into(),
            devnode: Some(format!("/dev/{}", path.trim_start_matches('/')).into()),
            devnum: None,
            seat: SeatName::parse(seat).unwrap(),
            kind: DeviceKind::Drm,
            uaccess: false,
            master_of_seat: master,
        }
    }

    #[test]
    fn first_and_last_master_define_seat_lifetime() {
        let mut model = Model::default();
        let first = device("/gpu0", "seat0", true);
        assert_eq!(
            model.add_or_change(first.clone()),
            vec![
                Event::SeatAdded(SeatName::seat0()),
                Event::DeviceAdded(first.clone())
            ]
        );
        let second = device("/gpu1", "seat0", true);
        assert_eq!(
            model.add_or_change(second.clone()),
            vec![Event::DeviceAdded(second)]
        );
        assert_eq!(model.remove(Path::new("/gpu0")).len(), 1);
        assert_eq!(
            model.remove(Path::new("/gpu1")),
            vec![
                Event::DeviceRemoved(device("/gpu1", "seat0", true)),
                Event::SeatRemoved(SeatName::seat0())
            ]
        );
    }

    #[test]
    fn move_reports_old_and_new_devices_before_lifecycle_changes() {
        let mut model = Model::default();
        let old = device("/gpu", "seat0", true);
        model.seed(old.clone());
        let moved = device("/gpu", "seat1", true);
        assert_eq!(
            model.add_or_change(moved.clone()),
            vec![
                Event::DeviceMoved { old, new: moved },
                Event::SeatRemoved(SeatName::seat0()),
                Event::SeatAdded(SeatName::parse("seat1").unwrap())
            ]
        );
    }

    #[test]
    fn same_seat_change_reports_old_and_new_and_updates_classification() {
        let mut model = Model::default();
        let mut old = device("/event0", "seat0", false);
        old.kind = DeviceKind::Evdev;
        old.uaccess = true;
        model.seed(old.clone());

        let mut changed = old.clone();
        changed.kind = DeviceKind::Other;
        assert_eq!(
            model.add_or_change(changed.clone()),
            vec![Event::DeviceChanged {
                old,
                new: changed.clone()
            }]
        );
        assert!(model.brokered_devices(&SeatName::seat0()).is_empty());
        assert_eq!(
            model.direct_access_devices(&SeatName::seat0()),
            vec![changed]
        );
    }

    #[test]
    fn same_seat_master_changes_report_seat_lifecycle() {
        let mut model = Model::default();
        let old = device("/gpu", "seat1", false);
        model.seed(old.clone());
        let mut master = old.clone();
        master.master_of_seat = true;
        assert_eq!(
            model.add_or_change(master.clone()),
            vec![
                Event::SeatAdded(SeatName::parse("seat1").unwrap()),
                Event::DeviceChanged {
                    old: old.clone(),
                    new: master.clone()
                }
            ]
        );
        assert_eq!(
            model.add_or_change(old.clone()),
            vec![
                Event::DeviceChanged {
                    old: master,
                    new: old
                },
                Event::SeatRemoved(SeatName::parse("seat1").unwrap())
            ]
        );
    }

    #[test]
    fn queued_monitor_changes_override_enumeration_and_duplicates_remain_idempotent() {
        let mut model = Model::default();
        let initial = device("/gpu", "seat0", true);
        // The monitor is opened first; enumeration can seed a stale record.
        model.seed(initial.clone());
        let moved = device("/gpu", "seat1", true);
        assert!(
            matches!(model.add_or_change(moved.clone()).first(), Some(Event::DeviceMoved { old, new })
            if old == &initial && new == &moved)
        );
        assert_eq!(
            model.seats(),
            BTreeSet::from([SeatName::parse("seat1").unwrap()])
        );
        assert!(model.add_or_change(moved.clone()).is_empty());
        assert_eq!(model.by_syspath(Path::new("/gpu")), Some(moved));
    }

    #[test]
    fn duplicate_change_is_ignored() {
        let mut model = Model::default();
        let device = device("/gpu", "seat0", true);
        model.seed(device.clone());
        assert!(model.add_or_change(device).is_empty());
    }

    #[test]
    fn direct_access_excludes_brokered_and_nodeless_devices() {
        let mut model = Model::default();
        let mut brokered = device("/event0", "seat0", false);
        brokered.kind = DeviceKind::Evdev;
        brokered.uaccess = true;
        model.seed(brokered.clone());

        let mut direct = device("/audio0", "seat0", false);
        direct.kind = DeviceKind::Other;
        direct.uaccess = true;
        model.seed(direct.clone());

        let mut nodeless = direct.clone();
        nodeless.syspath = "/audio-control".into();
        nodeless.devnode = None;
        model.seed(nodeless);

        assert_eq!(model.brokered_devices(&SeatName::seat0()), vec![brokered]);
        assert_eq!(
            model.direct_access_devices(&SeatName::seat0()),
            vec![direct]
        );
    }

    #[test]
    fn duplicate_remove_is_ignored() {
        let mut model = Model::default();
        assert!(model.remove(Path::new("/missing")).is_empty());
    }
}
