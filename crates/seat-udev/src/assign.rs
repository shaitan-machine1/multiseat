use crate::{SeatName, SeatNameError};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use thiserror::Error;
use udev::{Device, DeviceType, Enumerator, MonitorBuilder};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssignmentGroup {
    pub stable_id: String,
    pub ancestor: PathBuf,
    pub devices: Vec<PathBuf>,
    pub has_master: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedAssignment {
    pub stable_id: String,
    pub seat: SeatName,
    pub path: PathBuf,
}

#[derive(Debug, Error)]
pub enum AssignmentError {
    #[error("device {0} does not have a seat-tagged ancestor with ID_FOR_SEAT")]
    NotAssignable(PathBuf),
    #[error("invalid stable ID_FOR_SEAT {0:?}")]
    InvalidStableId(String),
    #[error("invalid persisted seat name: {0}")]
    InvalidSeat(#[from] SeatNameError),
    #[error("timed out waiting for synthetic udev event {uuid}; missing {missing:?}")]
    Timeout { uuid: Uuid, missing: Vec<PathBuf> },
    #[error("udev operation failed: {0}")]
    Udev(#[source] io::Error),
    #[error("command failed: {0}")]
    Command(String),
}

pub trait AssignmentBackend {
    type Guard<'a>
    where
        Self: 'a;

    fn lock(&self) -> Result<Self::Guard<'_>, AssignmentError>;
    fn resolve(&self, device: &Path) -> Result<AssignmentGroup, AssignmentError>;
    fn groups(&self) -> Result<Vec<AssignmentGroup>, AssignmentError>;
    fn write_rule(
        &self,
        group: &AssignmentGroup,
        seat: Option<&SeatName>,
    ) -> Result<(), AssignmentError>;
    fn reload(&self) -> Result<(), AssignmentError>;
    fn trigger_and_wait(
        &self,
        group: &AssignmentGroup,
        timeout: Duration,
    ) -> Result<(), AssignmentError>;
    fn persisted(&self) -> Result<Vec<PersistedAssignment>, AssignmentError>;
}

pub struct Assigner<B> {
    backend: B,
    timeout: Duration,
}

impl<B: AssignmentBackend> Assigner<B> {
    pub fn new(backend: B, timeout: Duration) -> Self {
        Self { backend, timeout }
    }

    pub fn assign(
        &self,
        device: &Path,
        seat: &SeatName,
    ) -> Result<AssignmentGroup, AssignmentError> {
        let _guard = self.backend.lock()?;
        let group = self.backend.resolve(device)?;
        self.backend.write_rule(&group, Some(seat))?;
        self.backend.reload()?;
        self.backend.trigger_and_wait(&group, self.timeout)?;
        Ok(group)
    }

    pub fn unassign(&self, device: &Path) -> Result<AssignmentGroup, AssignmentError> {
        let _guard = self.backend.lock()?;
        let group = self.backend.resolve(device)?;
        self.backend.write_rule(&group, None)?;
        self.backend.reload()?;
        self.backend.trigger_and_wait(&group, self.timeout)?;
        Ok(group)
    }

    pub fn flush(&self) -> Result<Vec<AssignmentGroup>, AssignmentError> {
        let _guard = self.backend.lock()?;
        let mut groups = self.backend.groups()?;
        // Persisted rules for currently unplugged groups must be removed too.
        // They have no sysfs devices to retrigger until the hardware returns.
        for record in self.backend.persisted()? {
            if !groups
                .iter()
                .any(|group| group.stable_id == record.stable_id)
            {
                groups.push(AssignmentGroup {
                    stable_id: record.stable_id,
                    ancestor: PathBuf::new(),
                    devices: Vec::new(),
                    has_master: false,
                });
            }
        }
        for group in &groups {
            self.backend.write_rule(group, None)?;
        }
        self.backend.reload()?;
        for group in groups.iter().filter(|group| !group.devices.is_empty()) {
            self.backend.trigger_and_wait(group, self.timeout)?;
        }
        Ok(groups)
    }

    pub fn persisted(&self) -> Result<Vec<PersistedAssignment>, AssignmentError> {
        self.backend.persisted()
    }
}

pub struct RealAssignmentBackend {
    rules_dir: PathBuf,
    lock_path: PathBuf,
}

impl RealAssignmentBackend {
    pub fn system() -> Self {
        Self {
            rules_dir: "/etc/udev/rules.d".into(),
            lock_path: "/run/seat-udev.lock".into(),
        }
    }

    pub fn new(rules_dir: PathBuf, lock_path: PathBuf) -> Self {
        Self {
            rules_dir,
            lock_path,
        }
    }

    fn rule_path(&self, stable_id: &str) -> PathBuf {
        self.rules_dir.join(format!("72-seat-{stable_id}.rules"))
    }
}

pub struct AssignmentLock(File);

impl Drop for AssignmentLock {
    fn drop(&mut self) {
        // SAFETY: `self.0` remains open throughout this call.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl AssignmentBackend for RealAssignmentBackend {
    type Guard<'a> = AssignmentLock;

    fn lock(&self) -> Result<Self::Guard<'_>, AssignmentError> {
        if let Some(parent) = self.lock_path.parent() {
            fs::create_dir_all(parent).map_err(AssignmentError::Udev)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&self.lock_path)
            .map_err(AssignmentError::Udev)?;
        // SAFETY: flock only reads the valid open descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
            return Err(AssignmentError::Udev(io::Error::last_os_error()));
        }
        Ok(AssignmentLock(file))
    }

    fn resolve(&self, path: &Path) -> Result<AssignmentGroup, AssignmentError> {
        resolve_group(path)
    }

    fn groups(&self) -> Result<Vec<AssignmentGroup>, AssignmentError> {
        let mut enumerator = Enumerator::new().map_err(AssignmentError::Udev)?;
        enumerator
            .match_tag("seat")
            .map_err(AssignmentError::Udev)?;
        let mut ids = BTreeSet::new();
        let mut groups = Vec::new();
        for device in enumerator.scan_devices().map_err(AssignmentError::Udev)? {
            let Some(id) = property(&device, "ID_FOR_SEAT") else {
                continue;
            };
            if ids.insert(id.clone()) {
                groups.push(group_from_ancestor(device, id)?);
            }
        }
        Ok(groups)
    }

    fn write_rule(
        &self,
        group: &AssignmentGroup,
        seat: Option<&SeatName>,
    ) -> Result<(), AssignmentError> {
        validate_stable_id(&group.stable_id)?;
        fs::create_dir_all(&self.rules_dir).map_err(AssignmentError::Udev)?;
        let path = self.rule_path(&group.stable_id);
        let Some(seat) = seat else {
            match fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(AssignmentError::Udev(error)),
            }
        };
        let temp = self.rules_dir.join(format!(
            ".72-seat-{}.rules.{}.tmp",
            group.stable_id,
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temp)
            .map_err(AssignmentError::Udev)?;
        let rule = rule_text(&group.stable_id, seat);
        if let Err(error) = (|| {
            file.write_all(rule.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })() {
            let _ = fs::remove_file(&temp);
            return Err(AssignmentError::Udev(error));
        }
        Ok(())
    }

    fn reload(&self) -> Result<(), AssignmentError> {
        let output = Command::new("udevadm")
            .args(["control", "--reload"])
            .output()
            .map_err(AssignmentError::Udev)?;
        if !output.status.success() {
            return Err(AssignmentError::Command(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        Ok(())
    }

    fn trigger_and_wait(
        &self,
        group: &AssignmentGroup,
        timeout: Duration,
    ) -> Result<(), AssignmentError> {
        let monitor = MonitorBuilder::new()
            .and_then(MonitorBuilder::listen)
            .map_err(AssignmentError::Udev)?;
        let uuid = Uuid::new_v4();
        let mut missing: BTreeSet<PathBuf> = group.devices.iter().cloned().collect();
        for syspath in &group.devices {
            fs::write(syspath.join("uevent"), format!("change {uuid}"))
                .map_err(AssignmentError::Udev)?;
        }

        let deadline = Instant::now() + timeout;
        while !missing.is_empty() {
            for event in monitor.iter() {
                if event
                    .property_value("SYNTH_UUID")
                    .map(|value| value == uuid.to_string().as_str())
                    .unwrap_or(false)
                {
                    missing.remove(event.syspath());
                }
            }
            if missing.is_empty() {
                break;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(AssignmentError::Timeout {
                    uuid,
                    missing: missing.into_iter().collect(),
                });
            };
            // SAFETY: the monitor owns this fd for the duration of poll.
            let borrowed = unsafe { BorrowedFd::borrow_raw(monitor.as_raw_fd()) };
            let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            let poll_timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
            if poll(&mut fds, poll_timeout).map_err(|error| AssignmentError::Udev(error.into()))?
                == 0
            {
                return Err(AssignmentError::Timeout {
                    uuid,
                    missing: missing.into_iter().collect(),
                });
            }
        }
        Ok(())
    }

    fn persisted(&self) -> Result<Vec<PersistedAssignment>, AssignmentError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.rules_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(AssignmentError::Udev(error)),
        };
        for entry in entries {
            let entry = entry.map_err(AssignmentError::Udev)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(id) = name
                .strip_prefix("72-seat-")
                .and_then(|value| value.strip_suffix(".rules"))
            else {
                continue;
            };
            let text = fs::read_to_string(entry.path()).map_err(AssignmentError::Udev)?;
            let Some(seat) = parse_rule_seat(&text) else {
                continue;
            };
            out.push(PersistedAssignment {
                stable_id: id.into(),
                seat: SeatName::parse(seat)?,
                path: entry.path(),
            });
        }
        out.sort_by(|a, b| a.stable_id.cmp(&b.stable_id));
        Ok(out)
    }
}

fn resolve_group(path: &Path) -> Result<AssignmentGroup, AssignmentError> {
    let mut device = device_from_path(path)?;
    loop {
        if has_tag(&device, "seat") {
            if let Some(id) = property(&device, "ID_FOR_SEAT") {
                return group_from_ancestor(device, id);
            }
        }
        let Some(parent) = device.parent() else {
            return Err(AssignmentError::NotAssignable(path.into()));
        };
        device = parent;
    }
}

fn group_from_ancestor(
    ancestor: Device,
    stable_id: String,
) -> Result<AssignmentGroup, AssignmentError> {
    validate_stable_id(&stable_id)?;
    let mut enumerator = Enumerator::new().map_err(AssignmentError::Udev)?;
    enumerator
        .match_parent(&ancestor)
        .map_err(AssignmentError::Udev)?;
    let mut devices = vec![ancestor.syspath().to_path_buf()];
    let mut has_master = has_tag(&ancestor, "master-of-seat");
    for device in enumerator.scan_devices().map_err(AssignmentError::Udev)? {
        devices.push(device.syspath().to_path_buf());
        has_master |= has_tag(&device, "master-of-seat");
    }
    devices.sort();
    devices.dedup();
    Ok(AssignmentGroup {
        stable_id,
        ancestor: ancestor.syspath().to_path_buf(),
        devices,
        has_master,
    })
}

fn device_from_path(path: &Path) -> Result<Device, AssignmentError> {
    let canonical = fs::canonicalize(path).map_err(AssignmentError::Udev)?;
    if canonical.starts_with("/sys") {
        return Device::from_syspath(&canonical).map_err(AssignmentError::Udev);
    }
    let metadata = fs::metadata(&canonical).map_err(AssignmentError::Udev)?;
    let kind = if metadata.file_type().is_char_device() {
        DeviceType::Character
    } else if metadata.file_type().is_block_device() {
        DeviceType::Block
    } else {
        return Err(AssignmentError::NotAssignable(path.into()));
    };
    Device::from_devnum(kind, metadata.rdev() as libc::dev_t).map_err(AssignmentError::Udev)
}

fn property(device: &Device, name: &str) -> Option<String> {
    device
        .property_value(name)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
}

fn has_tag(device: &Device, wanted: &str) -> bool {
    ["CURRENT_TAGS", "TAGS"].into_iter().any(|name| {
        property(device, name)
            .map(|tags| tags.split(':').any(|tag| tag == wanted))
            .unwrap_or(false)
    })
}

fn validate_stable_id(value: &str) -> Result<(), AssignmentError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(AssignmentError::InvalidStableId(value.into()));
    }
    Ok(())
}

fn rule_text(stable_id: &str, seat: &SeatName) -> String {
    format!(
        "# Generated by loginctl; persistent udev seat assignment.\nTAG==\"seat\", ENV{{ID_FOR_SEAT}}==\"{stable_id}\", ENV{{ID_SEAT}}=\"{seat}\"\n"
    )
}

fn parse_rule_seat(rule: &str) -> Option<String> {
    let marker = "ENV{ID_SEAT}=\"";
    let rest = rule.split(marker).nth(1)?;
    Some(rest.split('"').next()?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        groups: Vec<AssignmentGroup>,
        records: Vec<PersistedAssignment>,
    }

    impl AssignmentBackend for Fake {
        type Guard<'a> = ();

        fn lock(&self) -> Result<(), AssignmentError> {
            self.calls.borrow_mut().push("lock".into());
            Ok(())
        }
        fn resolve(&self, _device: &Path) -> Result<AssignmentGroup, AssignmentError> {
            self.calls.borrow_mut().push("resolve".into());
            Ok(self.groups[0].clone())
        }
        fn groups(&self) -> Result<Vec<AssignmentGroup>, AssignmentError> {
            self.calls.borrow_mut().push("groups".into());
            Ok(self.groups.clone())
        }
        fn write_rule(
            &self,
            _group: &AssignmentGroup,
            seat: Option<&SeatName>,
        ) -> Result<(), AssignmentError> {
            self.calls.borrow_mut().push(format!(
                "write:{}",
                seat.map(SeatName::as_str).unwrap_or("none")
            ));
            Ok(())
        }
        fn reload(&self) -> Result<(), AssignmentError> {
            self.calls.borrow_mut().push("reload".into());
            Ok(())
        }
        fn trigger_and_wait(
            &self,
            _group: &AssignmentGroup,
            _timeout: Duration,
        ) -> Result<(), AssignmentError> {
            self.calls.borrow_mut().push("trigger".into());
            Ok(())
        }
        fn persisted(&self) -> Result<Vec<PersistedAssignment>, AssignmentError> {
            Ok(self.records.clone())
        }
    }

    fn group() -> AssignmentGroup {
        AssignmentGroup {
            stable_id: "pci-0000_00_02_0".into(),
            ancestor: "/sys/gpu".into(),
            devices: vec!["/sys/gpu".into(), "/sys/gpu/drm/card0".into()],
            has_master: true,
        }
    }

    #[test]
    fn assign_is_locked_atomic_reload_then_correlated_trigger() {
        let fake = Fake {
            groups: vec![group()],
            ..Fake::default()
        };
        let assigner = Assigner::new(fake, Duration::from_secs(1));
        assigner
            .assign(
                Path::new("/dev/dri/card0"),
                &SeatName::parse("seat1").unwrap(),
            )
            .unwrap();
        assert_eq!(
            assigner.backend.calls.into_inner(),
            ["lock", "resolve", "write:seat1", "reload", "trigger"]
        );
    }

    #[test]
    fn flush_removes_each_known_group_before_one_reload() {
        let fake = Fake {
            groups: vec![
                group(),
                AssignmentGroup {
                    stable_id: "usb-1".into(),
                    ..group()
                },
            ],
            ..Fake::default()
        };
        let assigner = Assigner::new(fake, Duration::from_secs(1));
        assigner.flush().unwrap();
        assert_eq!(
            assigner.backend.calls.into_inner(),
            [
                "lock",
                "groups",
                "write:none",
                "write:none",
                "reload",
                "trigger",
                "trigger"
            ]
        );
    }

    #[test]
    fn flush_also_removes_unplugged_persisted_groups_without_triggering_them() {
        let fake = Fake {
            groups: vec![group()],
            records: vec![PersistedAssignment {
                stable_id: "usb-unplugged".into(),
                seat: SeatName::parse("seat1").unwrap(),
                path: "/rules/72-seat-usb-unplugged.rules".into(),
            }],
            ..Fake::default()
        };
        let assigner = Assigner::new(fake, Duration::from_secs(1));
        let groups = assigner.flush().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            assigner.backend.calls.into_inner(),
            [
                "lock",
                "groups",
                "write:none",
                "write:none",
                "reload",
                "trigger"
            ]
        );
    }

    #[test]
    fn real_backend_persists_lists_and_removes_rules_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let backend =
            RealAssignmentBackend::new(temp.path().join("rules"), temp.path().join("run/lock"));
        let group = group();
        backend
            .write_rule(&group, Some(&SeatName::parse("seat1").unwrap()))
            .unwrap();
        let records = backend.persisted().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].stable_id, group.stable_id);
        assert_eq!(records[0].seat.as_str(), "seat1");
        backend.write_rule(&group, None).unwrap();
        assert!(backend.persisted().unwrap().is_empty());
    }

    #[test]
    fn generated_rule_is_logind_compatible_and_parseable() {
        let text = rule_text("pci-0000_00_02_0", &SeatName::parse("seat1").unwrap());
        assert!(text.contains("TAG==\"seat\""));
        assert!(text.contains("ENV{ID_FOR_SEAT}==\"pci-0000_00_02_0\""));
        assert_eq!(parse_rule_seat(&text).as_deref(), Some("seat1"));
        assert!(validate_stable_id("../../bad").is_err());
    }
}
