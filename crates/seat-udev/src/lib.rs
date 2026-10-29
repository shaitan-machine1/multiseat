//! Portable semantic seat/device model backed by Linux libudev.
//!
//! No libudev type appears in the public API. The pure [`Model`] is suitable
//! for daemon logic and tests; [`Context`] owns the real monitor-first cache.

mod model;
mod real;
mod seat;

#[cfg(feature = "assign")]
pub mod assign;

pub use model::{DeviceKind, Event, Filter, Model, SeatDevice};
pub use real::{Context, Error};
pub use seat::{SeatName, SeatNameError};
