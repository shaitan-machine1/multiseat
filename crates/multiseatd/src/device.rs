use crate::authority::AuthorizedDevice;
use seat_udev::DeviceKind;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

nix::ioctl_write_int_bad!(evio_revoke, nix::request_code_write!(b'E', 0x91, 4));
nix::ioctl_write_int_bad!(hidio_revoke, nix::request_code_write!(b'H', 0x0d, 4));
nix::ioctl_none_bad!(drm_set_master, nix::request_code_none!(b'd', 0x1e));
nix::ioctl_none_bad!(drm_drop_master, nix::request_code_none!(b'd', 0x1f));

pub trait DeviceOps {
    fn open(&self, device: &AuthorizedDevice) -> io::Result<OwnedFd>;
    fn revoke(&self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()>;
}

#[derive(Default)]
pub struct RealDeviceOps;

impl DeviceOps for RealDeviceOps {
    fn open(&self, device: &AuthorizedDevice) -> io::Result<OwnedFd> {
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
        if device.kind == DeviceKind::Drm {
            // Render nodes and an already-held master can reject this; as in
            // upstream seatd, opening the authorized fd still succeeds.
            let _ = unsafe { drm_set_master(fd.as_raw_fd()) };
        }
        Ok(fd)
    }

    fn revoke(&self, kind: DeviceKind, fd: &OwnedFd) -> io::Result<()> {
        let result = unsafe {
            match kind {
                DeviceKind::Drm => drm_drop_master(fd.as_raw_fd()),
                DeviceKind::Evdev => evio_revoke(fd.as_raw_fd(), 0),
                DeviceKind::Hidraw => hidio_revoke(fd.as_raw_fd(), 0),
                DeviceKind::Other => return Ok(()),
            }
        };
        result.map(|_| ()).map_err(io::Error::from)
    }
}
