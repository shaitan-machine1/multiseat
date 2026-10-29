use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

nix::ioctl_write_int_bad!(evio_revoke, nix::request_code_write!(b'E', 0x91, 4));
nix::ioctl_write_int_bad!(hidio_revoke, nix::request_code_write!(b'H', 0x0d, 4));
nix::ioctl_none_bad!(drm_set_master, nix::request_code_none!(b'd', 0x1e));
nix::ioctl_none_bad!(drm_drop_master, nix::request_code_none!(b'd', 0x1f));

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    Drm,
    Evdev,
    Hidraw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllowedDevice {
    pub id: DeviceId,
    pub path: PathBuf,
    pub device_number: u64,
    pub kind: DeviceKind,
}

impl AllowedDevice {
    pub fn canonicalized(
        id: DeviceId,
        path: impl AsRef<Path>,
        device_number: u64,
        kind: DeviceKind,
    ) -> io::Result<Self> {
        Ok(Self {
            id,
            path: fs::canonicalize(path)?,
            device_number,
            kind,
        })
    }
}

pub trait DeviceOps: Send + 'static {
    fn open(&mut self, device: &AllowedDevice) -> io::Result<OwnedFd>;
    fn deactivate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()>;
    fn activate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()>;
}

impl<T: DeviceOps + ?Sized> DeviceOps for Box<T> {
    fn open(&mut self, device: &AllowedDevice) -> io::Result<OwnedFd> {
        (**self).open(device)
    }

    fn deactivate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()> {
        (**self).deactivate(kind, fd)
    }

    fn activate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()> {
        (**self).activate(kind, fd)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HidrawRevocation {
    Unknown,
    Supported,
    Unsupported,
}

pub struct KernelDeviceOps {
    hidraw_revocation: HidrawRevocation,
}

impl Default for KernelDeviceOps {
    fn default() -> Self {
        Self {
            hidraw_revocation: HidrawRevocation::Unknown,
        }
    }
}

impl KernelDeviceOps {
    fn open_validated(device: &AllowedDevice) -> io::Result<OwnedFd> {
        let fd = rustix::fs::open(
            &device.path,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOCTTY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(io::Error::from)?;

        // SAFETY: fstat writes one initialized stat structure for this live FD.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFCHR {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        if stat.st_rdev as u64 != device.device_number {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(fd)
    }

    fn ensure_hidraw_revocation(&mut self, device: &AllowedDevice) -> io::Result<()> {
        match self.hidraw_revocation {
            HidrawRevocation::Supported => return Ok(()),
            HidrawRevocation::Unsupported => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "running kernel does not support HIDIOCREVOKE",
                ));
            }
            HidrawRevocation::Unknown => {}
        }

        let probe = Self::open_validated(device)?;
        let result = unsafe { hidio_revoke(probe.as_raw_fd(), 0) };
        match result {
            Ok(_) => {
                self.hidraw_revocation = HidrawRevocation::Supported;
                Ok(())
            }
            Err(nix::errno::Errno::ENOTTY) => {
                self.hidraw_revocation = HidrawRevocation::Unsupported;
                tracing::error!(
                    device = %device.path.display(),
                    "denying hidraw access because HIDIOCREVOKE is unsupported"
                );
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "running kernel does not support HIDIOCREVOKE",
                ))
            }
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

impl DeviceOps for KernelDeviceOps {
    fn open(&mut self, device: &AllowedDevice) -> io::Result<OwnedFd> {
        if device.kind == DeviceKind::Hidraw {
            self.ensure_hidraw_revocation(device)?;
        }
        let fd = Self::open_validated(device)?;
        if device.kind == DeviceKind::Drm {
            // Render nodes and an already-held master may reject this. The C
            // server still returns the authorized descriptor in that case.
            let _ = unsafe { drm_set_master(fd.as_raw_fd()) };
        }
        Ok(fd)
    }

    fn deactivate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()> {
        let result = unsafe {
            match kind {
                DeviceKind::Drm => drm_drop_master(fd.as_raw_fd()),
                DeviceKind::Evdev => evio_revoke(fd.as_raw_fd(), 0),
                DeviceKind::Hidraw => hidio_revoke(fd.as_raw_fd(), 0),
            }
        };
        result.map(|_| ()).map_err(io::Error::from)
    }

    fn activate(&mut self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()> {
        match kind {
            DeviceKind::Drm => unsafe { drm_set_master(fd.as_raw_fd()) }
                .map(|_| ())
                .map_err(io::Error::from),
            DeviceKind::Evdev | DeviceKind::Hidraw => {
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn allowed_device_canonicalizes_identity() {
        let metadata = fs::metadata("/dev/null").unwrap();
        let device = AllowedDevice::canonicalized(
            DeviceId::new("null"),
            "/dev/null",
            metadata.rdev(),
            DeviceKind::Evdev,
        )
        .unwrap();
        assert_eq!(device.path, Path::new("/dev/null"));
        assert_eq!(device.device_number, metadata.rdev());
    }

    #[test]
    fn post_open_device_number_must_match() {
        let mut ops = KernelDeviceOps::default();
        let device = AllowedDevice {
            id: DeviceId::new("wrong"),
            path: "/dev/null".into(),
            device_number: u64::MAX,
            kind: DeviceKind::Evdev,
        };
        assert_eq!(
            ops.open(&device).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[test]
    fn hidraw_open_fails_closed_when_revocation_ioctl_is_unsupported() {
        // /dev/null is a real character device which returns ENOTTY for the
        // hidraw ioctl, so this exercises the runtime probe without hardware.
        let device = AllowedDevice::canonicalized(
            DeviceId::new("no-hidraw-revoke"),
            "/dev/null",
            fs::metadata("/dev/null").unwrap().rdev(),
            DeviceKind::Hidraw,
        )
        .unwrap();
        let mut ops = KernelDeviceOps::default();
        assert_eq!(
            ops.open(&device).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            ops.open(&device).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn opened_descriptors_have_defensive_flags() {
        let device = AllowedDevice::canonicalized(
            DeviceId::new("null"),
            "/dev/null",
            fs::metadata("/dev/null").unwrap().rdev(),
            DeviceKind::Evdev,
        )
        .unwrap();
        let fd = KernelDeviceOps::default().open(&device).unwrap();
        let descriptor_flags = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFD).unwrap();
        let status_flags = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(status_flags & libc::O_NONBLOCK, 0);
        assert_eq!(status_flags & libc::O_ACCMODE, libc::O_RDWR);
    }

    #[test]
    fn a_canonical_path_replaced_by_a_symlink_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replaced-device");
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        let device = AllowedDevice {
            id: DeviceId::new("replaced"),
            path,
            device_number: fs::metadata("/dev/null").unwrap().rdev(),
            kind: DeviceKind::Evdev,
        };
        assert_eq!(
            KernelDeviceOps::default()
                .open(&device)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
    }

    #[test]
    fn non_device_paths_are_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let device = AllowedDevice {
            id: DeviceId::new("file"),
            path: file.path().into(),
            device_number: 0,
            kind: DeviceKind::Evdev,
        };
        assert_eq!(
            KernelDeviceOps::default()
                .open(&device)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENODEV)
        );
    }
}
