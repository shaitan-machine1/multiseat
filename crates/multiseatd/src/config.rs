use seat_udev::SeatName;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DEVICE_SEPARATOR: char = ',';

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SeatConfig {
    pub name: SeatName,
    pub socket: PathBuf,
    pub vt_bound: bool,
    pub devices: BTreeSet<PathBuf>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("path is not valid UTF-8: {0:?}")]
    NonUtf8Path(PathBuf),
    #[error("device path contains unsupported {character:?}: {path:?}")]
    UnsupportedCharacter { path: PathBuf, character: char },
    #[error("could not serialize normalized seat configuration: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl SeatConfig {
    pub fn new(
        name: SeatName,
        runtime_dir: &Path,
        vt_bound: bool,
        devices: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, ConfigError> {
        let config = Self {
            socket: runtime_dir.join(format!("{name}.sock")),
            name,
            vt_bound,
            devices: devices.into_iter().collect(),
        };
        config.device_list()?;
        Ok(config)
    }

    pub fn device_list(&self) -> Result<String, ConfigError> {
        let mut values = Vec::with_capacity(self.devices.len());
        for path in &self.devices {
            let value = path
                .to_str()
                .ok_or_else(|| ConfigError::NonUtf8Path(path.clone()))?;
            for character in [DEVICE_SEPARATOR, '\n', '\r', '\0'] {
                if value.contains(character) {
                    return Err(ConfigError::UnsupportedCharacter {
                        path: path.clone(),
                        character,
                    });
                }
            }
            values.push(value);
        }
        Ok(values.join(&DEVICE_SEPARATOR.to_string()))
    }

    pub fn manifest(&self) -> Result<Vec<u8>, ConfigError> {
        let mut value = serde_json::to_vec_pretty(self)?;
        value.push(b'\n');
        Ok(value)
    }

    pub fn hash(&self) -> Result<String, ConfigError> {
        let digest = Sha256::digest(self.manifest()?);
        Ok(hex::encode(&digest[..12]))
    }

    pub fn service_name(&self) -> Result<String, ConfigError> {
        Ok(format!("{}-{}", self.name, self.hash()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_is_sorted_and_hash_is_stable() {
        let first = SeatConfig::new(
            SeatName::parse("seat1").unwrap(),
            Path::new("/run/test"),
            false,
            [
                PathBuf::from("/dev/input/event2"),
                PathBuf::from("/dev/dri/card1"),
            ],
        )
        .unwrap();
        let second = SeatConfig::new(
            SeatName::parse("seat1").unwrap(),
            Path::new("/run/test"),
            false,
            [
                PathBuf::from("/dev/dri/card1"),
                PathBuf::from("/dev/input/event2"),
            ],
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.hash().unwrap(), second.hash().unwrap());
        assert_eq!(
            first.device_list().unwrap(),
            "/dev/dri/card1,/dev/input/event2"
        );
    }

    #[test]
    fn separator_is_rejected() {
        let error = SeatConfig::new(
            SeatName::seat0(),
            Path::new("/run/test"),
            true,
            [PathBuf::from("/dev/input/event,1")],
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::UnsupportedCharacter { .. }));
    }
}
