use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A udev/logind-compatible seat name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SeatName(String);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SeatNameError {
    #[error("seat name must start with 'seat'")]
    MissingPrefix,
    #[error("seat name is too long (maximum 63 bytes)")]
    TooLong,
    #[error("seat name contains invalid character {0:?}")]
    InvalidCharacter(char),
}

impl SeatName {
    pub const MAX_LEN: usize = 63;

    pub fn parse(value: impl Into<String>) -> Result<Self, SeatNameError> {
        let value = value.into();
        if !value.starts_with("seat") {
            return Err(SeatNameError::MissingPrefix);
        }
        if value.len() > Self::MAX_LEN {
            return Err(SeatNameError::TooLong);
        }
        if let Some(ch) = value[4..]
            .chars()
            .find(|ch| !ch.is_ascii_alphanumeric() && *ch != '_' && *ch != '-')
        {
            return Err(SeatNameError::InvalidCharacter(ch));
        }
        Ok(Self(value))
    }

    pub fn seat0() -> Self {
        Self("seat0".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SeatName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SeatName {
    type Err = SeatNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl From<SeatName> for String {
    fn from(value: SeatName) -> Self {
        value.0
    }
}

impl TryFrom<String> for SeatName {
    type Error = SeatNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_portable_names() {
        for name in ["seat", "seat0", "seat-usb_2", "seatABC"] {
            assert_eq!(SeatName::parse(name).unwrap().as_str(), name);
        }
        assert!(matches!(
            SeatName::parse("other0"),
            Err(SeatNameError::MissingPrefix)
        ));
        assert!(matches!(
            SeatName::parse("seat.a"),
            Err(SeatNameError::InvalidCharacter('.'))
        ));
    }
}
