use crate::config::{ConfigError, SeatConfig};
use seat_udev::SeatName;
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeatInstance {
    pub config: SeatConfig,
    pub service_dir: PathBuf,
    pub ready: bool,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("service I/O failed for {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid generated manifest {path:?}: {source}")]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid service link name {0}")]
    InvalidSeat(String),
    #[error("generated service hash collision at {0:?}")]
    HashCollision(PathBuf),
    #[error("{program} failed with status {status}: {stderr}")]
    Command {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("seatd socket is not a mode-0600 Unix socket: {0:?}")]
    InvalidSocket(PathBuf),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

pub trait SeatServiceManager: Send + Sync {
    fn list(&self) -> Result<Vec<SeatInstance>, ServiceError>;
    fn start(&self, config: &SeatConfig) -> Result<(), ServiceError>;
    fn stop(&self, seat: &SeatName) -> Result<(), ServiceError>;
    fn status(&self, seat: &SeatName) -> Result<Option<SeatInstance>, ServiceError>;
}

pub struct S6SeatServiceManager {
    runtime_dir: PathBuf,
    scandir: PathBuf,
    services_dir: PathBuf,
    seatd_program: PathBuf,
    wait_milliseconds: u64,
}

impl S6SeatServiceManager {
    pub fn new(
        runtime_dir: PathBuf,
        seatd_program: PathBuf,
        wait_milliseconds: u64,
    ) -> Result<Self, ServiceError> {
        let manager = Self {
            scandir: runtime_dir.join("scandir"),
            services_dir: runtime_dir.join("services"),
            runtime_dir,
            seatd_program,
            wait_milliseconds,
        };
        manager.create_private_directory(&manager.runtime_dir)?;
        manager.create_private_directory(&manager.scandir)?;
        manager.create_private_directory(&manager.services_dir)?;
        Ok(manager)
    }

    fn create_private_directory(&self, path: &Path) -> Result<(), ServiceError> {
        fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(path, source))
    }

    fn link_path(&self, seat: &SeatName) -> PathBuf {
        self.scandir.join(seat.as_str())
    }

    fn prepare_service(&self, config: &SeatConfig) -> Result<PathBuf, ServiceError> {
        let destination = self.services_dir.join(config.service_name()?);
        if destination.exists() {
            let deployed = read_manifest(&destination)?;
            return if deployed == *config {
                Ok(destination)
            } else {
                Err(ServiceError::HashCollision(destination))
            };
        }

        let temporary = self.services_dir.join(format!(
            ".{}.tmp-{}",
            config.service_name()?,
            std::process::id()
        ));
        if temporary.exists() {
            fs::remove_dir_all(&temporary).map_err(|source| io_error(&temporary, source))?;
        }
        fs::create_dir(&temporary).map_err(|source| io_error(&temporary, source))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(&temporary, source))?;
        let env_dir = temporary.join("env");
        fs::create_dir(&env_dir).map_err(|source| io_error(&env_dir, source))?;

        write_file(
            &env_dir.join("SEATD_SEAT"),
            config.name.as_str().as_bytes(),
            0o600,
        )?;
        write_file(
            &env_dir.join("SEATD_SOCK"),
            path_bytes(&config.socket)?.as_bytes(),
            0o600,
        )?;
        write_file(
            &env_dir.join("SEATD_VTBOUND"),
            if config.vt_bound { b"1" } else { b"0" },
            0o600,
        )?;
        let devices = config.device_list()?;
        // s6-envdir treats a zero-length file as "unset". A newline-only
        // file deliberately sets an empty allowlist, which seatd interprets
        // as deny-all.
        write_file(
            &env_dir.join("SEATD_DEVICES"),
            if devices.is_empty() {
                b"\n"
            } else {
                devices.as_bytes()
            },
            0o600,
        )?;
        write_file(&temporary.join("notification-fd"), b"3\n", 0o600)?;
        write_file(&temporary.join("config.json"), &config.manifest()?, 0o600)?;

        let program = shell_quote(path_bytes(&self.seatd_program)?);
        let run = format!("#!/bin/sh\nexec 2>&1\nexec /usr/bin/s6-envdir ./env {program} -n 3\n");
        write_file(&temporary.join("run"), run.as_bytes(), 0o700)?;
        fs::rename(&temporary, &destination).map_err(|source| io_error(&destination, source))?;
        Ok(destination)
    }

    fn command(&self, program: &str, arguments: &[&Path]) -> Result<Output, ServiceError> {
        let mut command = Command::new(program);
        command.args(arguments);
        command
            .output()
            .map_err(|source| io_error(Path::new(program), source))
    }

    fn checked(&self, program: &str, arguments: &[&Path]) -> Result<(), ServiceError> {
        let output = self.command(program, arguments)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ServiceError::Command {
                program: program.into(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
            })
        }
    }

    fn wait(&self, state: &str, link: &Path) -> Result<(), ServiceError> {
        let timeout = self.wait_milliseconds.to_string();
        let state = Path::new(state);
        self.checked(
            "s6-svwait",
            &[state, Path::new("-t"), Path::new(&timeout), link],
        )
    }

    fn is_ready(&self, link: &Path) -> Result<bool, ServiceError> {
        if !link.exists() {
            return Ok(false);
        }
        Ok(self
            .command("s6-svstat", &[Path::new("-u"), link])?
            .status
            .success())
    }

    fn verify_socket(&self, path: &Path) -> Result<(), ServiceError> {
        let metadata = fs::metadata(path).map_err(|source| io_error(path, source))?;
        if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(ServiceError::InvalidSocket(path.into()));
        }
        Ok(())
    }
}

impl SeatServiceManager for S6SeatServiceManager {
    fn list(&self) -> Result<Vec<SeatInstance>, ServiceError> {
        let mut instances = Vec::new();
        for entry in
            fs::read_dir(&self.scandir).map_err(|source| io_error(&self.scandir, source))?
        {
            let entry = entry.map_err(|source| io_error(&self.scandir, source))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let seat =
                SeatName::parse(&name).map_err(|_| ServiceError::InvalidSeat(name.clone()))?;
            let link = self.link_path(&seat);
            let target = fs::canonicalize(&link).map_err(|source| io_error(&link, source))?;
            let config = read_manifest(&target)?;
            let ready = self.is_ready(&link)? && self.verify_socket(&config.socket).is_ok();
            instances.push(SeatInstance {
                config,
                service_dir: target,
                ready,
            });
        }
        instances.sort_by(|left, right| left.config.name.cmp(&right.config.name));
        Ok(instances)
    }

    fn start(&self, config: &SeatConfig) -> Result<(), ServiceError> {
        if self.status(&config.name)?.is_some() {
            return Err(ServiceError::Command {
                program: "s6-svlink".into(),
                status: "seat already deployed".into(),
                stderr: config.name.to_string(),
            });
        }
        let service = self.prepare_service(config)?;
        let name = Path::new(config.name.as_str());
        self.checked("s6-svlink", &[&self.scandir, &service, name])?;
        let link = self.link_path(&config.name);
        if let Err(error) = self
            .wait("-U", &link)
            .and_then(|_| self.verify_socket(&config.socket))
        {
            let _ = self.checked("s6-svunlink", &[&self.scandir, name]);
            return Err(error);
        }
        Ok(())
    }

    fn stop(&self, seat: &SeatName) -> Result<(), ServiceError> {
        let Some(instance) = self.status(seat)? else {
            return Ok(());
        };
        let link = self.link_path(seat);
        self.checked("s6-svc", &[Path::new("-d"), &link])?;
        self.wait("-d", &link)?;
        self.checked("s6-svunlink", &[&self.scandir, Path::new(seat.as_str())])?;
        if instance.config.socket.exists() {
            fs::remove_file(&instance.config.socket)
                .map_err(|source| io_error(&instance.config.socket, source))?;
        }
        fs::remove_dir_all(&instance.service_dir)
            .map_err(|source| io_error(&instance.service_dir, source))?;
        Ok(())
    }

    fn status(&self, seat: &SeatName) -> Result<Option<SeatInstance>, ServiceError> {
        Ok(self
            .list()?
            .into_iter()
            .find(|instance| &instance.config.name == seat))
    }
}

fn read_manifest(service_dir: &Path) -> Result<SeatConfig, ServiceError> {
    let path = service_dir.join("config.json");
    let value = fs::read(&path).map_err(|source| io_error(&path, source))?;
    serde_json::from_slice(&value).map_err(|source| ServiceError::Manifest { path, source })
}

fn write_file(path: &Path, value: &[u8], mode: u32) -> Result<(), ServiceError> {
    fs::write(path, value).map_err(|source| io_error(path, source))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| io_error(path, source))
}

fn path_bytes(path: &Path) -> Result<&str, ConfigError> {
    path.to_str()
        .ok_or_else(|| ConfigError::NonUtf8Path(path.into()))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn io_error(path: &Path, source: std::io::Error) -> ServiceError {
    ServiceError::Io {
        path: path.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generated_service_is_immutable_and_complete() {
        let temporary = tempdir().unwrap();
        let manager = S6SeatServiceManager::new(
            temporary.path().join("run"),
            PathBuf::from("/test path/seatd"),
            100,
        )
        .unwrap();
        let config = SeatConfig::new(
            SeatName::parse("seat1").unwrap(),
            &manager.runtime_dir,
            false,
            [PathBuf::from("/dev/dri/card1")],
        )
        .unwrap();
        let first = manager.prepare_service(&config).unwrap();
        let second = manager.prepare_service(&config).unwrap();
        assert_eq!(first, second);
        assert_eq!(read_manifest(&first).unwrap(), config);
        assert_eq!(
            fs::read_to_string(first.join("env/SEATD_DEVICES")).unwrap(),
            "/dev/dri/card1"
        );
        let run = fs::read_to_string(first.join("run")).unwrap();
        assert!(run.contains("'/test path/seatd' -n 3"));
        assert_eq!(
            fs::metadata(first.join("run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let empty = SeatConfig::new(
            SeatName::parse("seat2").unwrap(),
            &manager.runtime_dir,
            false,
            [],
        )
        .unwrap();
        let empty_service = manager.prepare_service(&empty).unwrap();
        assert_eq!(
            fs::read(empty_service.join("env/SEATD_DEVICES")).unwrap(),
            b"\n"
        );
    }
}
