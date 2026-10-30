//! Linux seatd protocol server building blocks.
//!
//! The crate owns seatd wire handling, descriptor transport, device
//! validation and, as the implementation grows, one blocking actor per seat.
//! It intentionally has no dependency on udev, D-Bus, PAM or user sessions.

mod actor;
mod connection;
mod device;
pub mod protocol;
mod state;
mod vt;

pub use actor::{SeatConfig, SeatEvent, SeatHandle, ServerRuntime};
pub use connection::{Connection, ReadStatus};
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use device::NullDeviceOps;
pub use device::{AllowedDevice, DeviceId, DeviceKind, DeviceOps, KernelDeviceOps};
pub use state::{Action, ClientId, ClientState, SeatState, StateError};
pub use vt::{LinuxVtOps, VtOps};
