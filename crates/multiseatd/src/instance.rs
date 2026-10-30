//! Process exclusivity must precede topology, ACLs, actors and bus publication.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Keep this guard alive until ALL resource cleanup and actor joins finish.
/// Never unlink the file: unlinking a locked inode permits a competing lock.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe multiseatd lock file",
            ));
        }
        // SAFETY: file is a live owned descriptor; flock is released on close.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "cannot acquire multiseatd instance lock {}: {}",
                    path.display(),
                    io::Error::last_os_error()
                ),
            ));
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusivity_precedes_initialization_and_lasts_through_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.lock");
        let guard = InstanceLock::acquire(&path).unwrap();
        let initialized = std::cell::Cell::new(false);
        let result = InstanceLock::acquire(&path).inspect(|_| {
            initialized.set(true);
        });
        assert!(result.is_err());
        assert!(!initialized.get());
        assert!(InstanceLock::acquire(&path).is_err());
        drop(guard);
        assert!(InstanceLock::acquire(&path).is_ok());
        assert!(path.exists(), "never unlink the lock inode");
    }

    #[test]
    fn symlinks_and_writable_lock_files_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(InstanceLock::acquire(&path).is_err());
        let alias = directory.path().join("alias");
        symlink(path, &alias).unwrap();
        assert!(InstanceLock::acquire(&alias).is_err());
    }
}
