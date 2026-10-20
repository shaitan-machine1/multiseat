use seat_udev::{Context, DeviceKind, Error as UdevError, SeatName};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedDevice {
    pub path: PathBuf,
    pub devnum: u64,
    pub kind: DeviceKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenError(pub i32);

pub trait DeviceAuthority {
    fn authorize(&self, seat: &SeatName, requested: &Path) -> Result<AuthorizedDevice, OpenError>;
}

pub struct ContextAuthority {
    context: Rc<RefCell<Context>>,
}

impl ContextAuthority {
    pub fn new(context: Rc<RefCell<Context>>) -> Self {
        Self { context }
    }
}

impl DeviceAuthority for ContextAuthority {
    fn authorize(&self, seat: &SeatName, requested: &Path) -> Result<AuthorizedDevice, OpenError> {
        let device = self
            .context
            .borrow()
            .lookup_path(requested)
            .map_err(|error| match error {
                UdevError::NotInitialized(_) => OpenError(libc::EAGAIN),
                UdevError::NotFound(_) => OpenError(libc::ENOENT),
                _ => OpenError(libc::EIO),
            })?;
        if &device.seat != seat || !device.brokerable() {
            return Err(OpenError(libc::EPERM));
        }
        Ok(AuthorizedDevice {
            path: device.devnode.ok_or(OpenError(libc::ENOENT))?,
            devnum: device.devnum.ok_or(OpenError(libc::ENOENT))?,
            kind: device.kind,
        })
    }
}
