use crate::{DeviceKind, Event, Filter, Model, SeatDevice, SeatName, SeatNameError};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use thiserror::Error;
use udev::{Device, DeviceType, Enumerator, EventType, MonitorBuilder, MonitorSocket};

#[derive(Debug, Error)]
pub enum Error {
    #[error("device {0} does not exist or is not known to udev")]
    NotFound(PathBuf),
    #[error("device {0} has not completed udev initialization")]
    NotInitialized(PathBuf),
    #[error("device has invalid ID_SEAT: {0}")]
    InvalidSeat(#[from] SeatNameError),
    #[error("udev operation failed: {0}")]
    Udev(#[source] io::Error),
    #[error("could not inspect static uaccess node {path}: {source}")]
    StaticNode {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A monitor-first libudev context plus a pure cached seat model.
///
/// libudev values never cross this API. The underlying crate is intentionally
/// built without its `send` feature, keeping this context on one event-loop
/// thread.
pub struct Context {
    model: Model,
    monitor: MonitorSocket,
}

impl Context {
    pub fn new() -> Result<Self, Error> {
        // Listen before enumeration so changes that race the initial scan are
        // queued and reconciled afterward.
        let monitor = MonitorBuilder::new()
            .and_then(MonitorBuilder::listen)
            .map_err(Error::Udev)?;
        let mut model = Model::default();
        let mut enumerator = Enumerator::new().map_err(Error::Udev)?;
        enumerator.match_is_initialized().map_err(Error::Udev)?;
        for device in enumerator.scan_devices().map_err(Error::Udev)? {
            if let Ok(device) = semantic_device(&device) {
                if relevant(&device) {
                    model.seed(device);
                }
            }
        }
        let mut context = Self { model, monitor };
        // Reconcile events queued during enumeration before exposing the initial
        // cache. Daemon startup must not publish the pre-monitor snapshot.
        while context.receive()?.is_some() {}
        Ok(context)
    }

    pub fn monitor_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the returned descriptor cannot outlive this context, which
        // owns the monitor socket.
        unsafe { BorrowedFd::borrow_raw(self.monitor.as_raw_fd()) }
    }

    /// Drains one queued udev event. `Ok(None)` means the nonblocking monitor
    /// currently has no event. Ignored/uninitialized events produce an empty
    /// event vector.
    pub fn receive(&mut self) -> Result<Option<Vec<Event>>, Error> {
        let Some(event) = self.monitor.iter().next() else {
            return Ok(None);
        };
        let syspath = event.syspath().to_path_buf();
        let events = match event.event_type() {
            EventType::Remove => self.model.remove(&syspath),
            EventType::Add | EventType::Change | EventType::Bind | EventType::Unbind => {
                if !event.is_initialized() {
                    Vec::new()
                } else {
                    match semantic_device(&event) {
                        Ok(device) if relevant(&device) => self.model.add_or_change(device),
                        Ok(_) => self.model.remove(&syspath),
                        Err(Error::NotInitialized(_)) => Vec::new(),
                        Err(error) => return Err(error),
                    }
                }
            }
            EventType::Unknown => Vec::new(),
        };
        Ok(Some(events))
    }

    pub fn seats(&self) -> Vec<SeatName> {
        self.model.seats().into_iter().collect()
    }

    pub fn devices(&self, seat: &SeatName, filter: Filter) -> Vec<SeatDevice> {
        self.model.devices(seat, filter)
    }

    pub fn all_devices(&self) -> Vec<SeatDevice> {
        self.model.all_devices()
    }

    pub fn brokered_devices(&self, seat: &SeatName) -> Vec<SeatDevice> {
        self.model.brokered_devices(seat)
    }

    pub fn direct_access_devices(&self, seat: &SeatName) -> Vec<SeatDevice> {
        self.model.direct_access_devices(seat)
    }

    pub fn static_direct_access_nodes(&self, seat: &SeatName) -> Result<Vec<PathBuf>, Error> {
        if seat != &SeatName::seat0() {
            return Ok(Vec::new());
        }
        discover_static_direct_access_nodes(
            &self.model,
            Path::new("/run/udev/static_node-tags/uaccess"),
        )
    }

    pub fn lookup_syspath(&self, syspath: &Path) -> Result<SeatDevice, Error> {
        if let Some(device) = self.model.by_syspath(syspath) {
            return Ok(device);
        }
        let device = Device::from_syspath(syspath).map_err(|_| Error::NotFound(syspath.into()))?;
        semantic_device(&device)
    }

    pub fn lookup_devnum(&self, devnum: u64) -> Result<SeatDevice, Error> {
        if let Some(device) = self.model.by_devnum(devnum) {
            return Ok(device);
        }
        for kind in [DeviceType::Character, DeviceType::Block] {
            if let Ok(device) = Device::from_devnum(kind, devnum as libc::dev_t) {
                return semantic_device(&device);
            }
        }
        Err(Error::NotFound(format!("device number {devnum}").into()))
    }

    pub fn lookup_path(&self, path: &Path) -> Result<SeatDevice, Error> {
        let canonical = fs::canonicalize(path).map_err(|_| Error::NotFound(path.into()))?;
        if let Some(device) = self.model.by_devnode(&canonical) {
            return Ok(device);
        }
        if canonical.starts_with("/sys") {
            return self.lookup_syspath(&canonical);
        }
        let metadata = fs::metadata(&canonical).map_err(|_| Error::NotFound(path.into()))?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_char_device() {
            DeviceType::Character
        } else if file_type.is_block_device() {
            DeviceType::Block
        } else {
            return Err(Error::NotFound(path.into()));
        };
        let device = Device::from_devnum(kind, metadata.rdev() as libc::dev_t)
            .map_err(|_| Error::NotFound(path.into()))?;
        semantic_device(&device)
    }
}

fn discover_static_direct_access_nodes(
    model: &Model,
    directory: &Path,
) -> Result<Vec<PathBuf>, Error> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::StaticNode {
                path: directory.into(),
                source,
            });
        }
    };
    let mut nodes = std::collections::BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::StaticNode {
            path: directory.into(),
            source,
        })?;
        let entry_path = entry.path();
        let path = match fs::canonicalize(&entry_path) {
            Ok(path) => path,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::StaticNode {
                    path: entry_path,
                    source,
                });
            }
        };
        if model
            .by_devnode(&path)
            .is_some_and(|device| device.brokerable())
            || fs::metadata(&path)
                .ok()
                .and_then(|metadata| model.by_devnum(metadata.rdev()))
                .is_some_and(|device| device.brokerable())
        {
            continue;
        }
        nodes.insert(path);
    }
    Ok(nodes.into_iter().collect())
}

fn semantic_device(device: &Device) -> Result<SeatDevice, Error> {
    let syspath = device.syspath().to_path_buf();
    if !device.is_initialized() {
        return Err(Error::NotInitialized(syspath));
    }
    let seat = match device.property_value("ID_SEAT") {
        Some(value) if !value.is_empty() => SeatName::parse(value.to_string_lossy().into_owned())?,
        _ => SeatName::seat0(),
    };
    let subsystem = device
        .subsystem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sysname = device.sysname().to_string_lossy();
    let kind = match subsystem.as_str() {
        "drm" => DeviceKind::Drm,
        "input" if sysname.starts_with("event") => DeviceKind::Evdev,
        "hidraw" => DeviceKind::Hidraw,
        _ => DeviceKind::Other,
    };
    Ok(SeatDevice {
        syspath,
        devnode: device
            .devnode()
            .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())),
        devnum: device.devnum(),
        seat,
        kind,
        uaccess: has_tag(device, "uaccess"),
        master_of_seat: has_tag(device, "master-of-seat"),
    })
}

fn relevant(device: &SeatDevice) -> bool {
    device.brokerable() || device.uaccess || device.master_of_seat
}

fn has_tag(device: &Device, wanted: &str) -> bool {
    ["CURRENT_TAGS", "TAGS"].into_iter().any(|property| {
        device
            .property_value(property)
            .map(|tags| tags.to_string_lossy().split(':').any(|tag| tag == wanted))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn static_nodes_are_sorted_deduplicated_and_exclude_brokered_devices() {
        let root = tempfile::tempdir().unwrap();
        let tags = root.path().join("tags");
        let devices = root.path().join("devices");
        fs::create_dir(&tags).unwrap();
        fs::create_dir(&devices).unwrap();

        let direct = devices.join("audio");
        let brokered = devices.join("event0");
        fs::write(&direct, b"").unwrap();
        fs::write(&brokered, b"").unwrap();
        symlink(&direct, tags.join("audio-a")).unwrap();
        symlink(&direct, tags.join("audio-b")).unwrap();
        symlink(&brokered, tags.join("event0")).unwrap();
        symlink(devices.join("missing"), tags.join("missing")).unwrap();

        let mut model = Model::default();
        model.seed(SeatDevice {
            syspath: "/sys/event0".into(),
            devnode: Some(fs::canonicalize(&brokered).unwrap()),
            devnum: Some(1),
            seat: SeatName::seat0(),
            kind: DeviceKind::Evdev,
            uaccess: true,
            master_of_seat: false,
        });

        assert_eq!(
            discover_static_direct_access_nodes(&model, &tags).unwrap(),
            vec![fs::canonicalize(direct).unwrap()]
        );
    }

    #[test]
    fn static_nodes_exclude_brokered_device_numbers_even_under_other_names() {
        let directory = tempfile::tempdir().unwrap();
        symlink("/dev/null", directory.path().join("alias")).unwrap();
        let mut model = Model::default();
        model.seed(SeatDevice {
            syspath: "/sys/test-input".into(),
            devnode: Some("/dev/another-name".into()),
            devnum: Some(fs::metadata("/dev/null").unwrap().rdev()),
            seat: SeatName::seat0(),
            kind: DeviceKind::Evdev,
            uaccess: true,
            master_of_seat: false,
        });
        assert!(
            discover_static_direct_access_nodes(&model, directory.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_static_node_directory_is_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            discover_static_direct_access_nodes(&Model::default(), &root.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }
}
