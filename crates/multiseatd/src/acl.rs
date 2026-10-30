//! Linux access-ACL rewriting, adapted from aclad's `src/acl.rs` at
//! b4347297c895c47074555ea8eb0a9432e8a3cb0e (GPL-3.0-only).
//! Static-node discovery belongs to seat-udev, not this backend.
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const ACL_TYPE_ACCESS: c_int = 0x8000;
type Acl = *mut c_void;

#[link(name = "acl")]
unsafe extern "C" {
    fn acl_get_file(path: *const c_char, acl_type: c_int) -> Acl;
    fn acl_from_text(text: *const c_char) -> Acl;
    fn acl_to_text(acl: Acl, length: *mut isize) -> *mut c_char;
    fn acl_calc_mask(acl: *mut Acl) -> c_int;
    fn acl_cmp(left: Acl, right: Acl) -> c_int;
    fn acl_set_file(path: *const c_char, acl_type: c_int, acl: Acl) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeIdentity {
    device: u64,
    inode: u64,
    number: u64,
    kind: u32,
}

/// Retain the identity even if a write's outcome is uncertain. Otherwise a
/// failed write to a replacement inode could be skipped by old-node cleanup.
#[derive(Debug)]
pub struct ApplyFailure {
    pub identity: Option<NodeIdentity>,
    pub error: io::Error,
}

impl From<io::Error> for ApplyFailure {
    fn from(error: io::Error) -> Self {
        Self {
            identity: None,
            error,
        }
    }
}

/// Internal daemon test seam. Production always uses Linux libacl.
pub trait AclOps {
    fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()>;

    /// Apply to one pinned node. For old-node cleanup, skip a replacement
    /// rather than clearing an unrelated inode. Fake backends need no inode.
    fn apply_users(
        &self,
        path: &Path,
        users: &BTreeSet<u32>,
        _expected: Option<NodeIdentity>,
    ) -> Result<Option<NodeIdentity>, ApplyFailure> {
        self.set_users(path, users)?;
        Ok(None)
    }
}

struct AclObject(Acl);

impl AclObject {
    // The caller supplies an owned allocation from libacl, or a null failure.
    fn owned(pointer: Acl) -> io::Result<Self> {
        if pointer.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(pointer))
        }
    }
}

impl Drop for AclObject {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns an allocation returned by libacl.
        unsafe { acl_free(self.0) };
    }
}

#[derive(Default)]
pub struct LibAcl;

impl AclOps for LibAcl {
    fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()> {
        self.apply_users(path, users, None)
            .map(|_| ())
            .map_err(|failure| failure.error)
    }

    fn apply_users(
        &self,
        path: &Path,
        users: &BTreeSet<u32>,
        expected: Option<NodeIdentity>,
    ) -> Result<Option<NodeIdentity>, ApplyFailure> {
        // O_PATH pins the inode without opening a device for I/O (which can
        // itself have driver side effects or fail EBUSY). libacl accesses the
        // pinned inode through procfs, never re-resolving the original path.
        let node = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = node.metadata()?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::from_raw_os_error(libc::ELOOP).into());
        }
        let identity = NodeIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            number: metadata.rdev(),
            kind: metadata.mode() & libc::S_IFMT,
        };
        if expected.is_some_and(|old| old != identity) {
            return Ok(None);
        }
        apply_pinned_users(&node, identity, users).map_err(|error| ApplyFailure {
            identity: Some(identity),
            error,
        })
    }
}

fn apply_pinned_users(
    node: &std::fs::File,
    identity: NodeIdentity,
    users: &BTreeSet<u32>,
) -> io::Result<Option<NodeIdentity>> {
    let path =
        CString::new(format!("/proc/self/fd/{}", node.as_raw_fd())).expect("FD path has no NUL");
    // SAFETY: path names the pinned live inode; the ACL allocation is owned.
    let current_acl = AclObject::owned(unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_ACCESS) })?;
    let current = acl_text(&current_acl)?;
    let rewritten = CString::new(rewrite_acl(&current, users))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    // SAFETY: rewritten is NUL terminated; libacl returns an owned ACL.
    let mut acl = AclObject::owned(unsafe { acl_from_text(rewritten.as_ptr()) })?;
    // SAFETY: acl is owned and writable; libacl may replace its allocation.
    if unsafe { acl_calc_mask(&mut acl.0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Revalidate every time, but avoid redundant xattr writes once the
    // actual ACL (not a pathname cache) matches the full desired state.
    // SAFETY: both ACL objects are live and owned by this scope.
    match unsafe { acl_cmp(current_acl.0, acl.0) } {
        0 => return Ok(Some(identity)),
        -1 => return Err(io::Error::last_os_error()),
        _ => {}
    }
    // SAFETY: path and acl are live for this call.
    if unsafe { acl_set_file(path.as_ptr(), ACL_TYPE_ACCESS, acl.0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(identity))
}

#[cfg(test)]
fn path_cstring(path: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
fn read_acl(path: &CStr) -> io::Result<String> {
    // SAFETY: path is NUL terminated; returned ACL is owned by this wrapper.
    let acl = AclObject::owned(unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_ACCESS) })?;
    acl_text(&acl)
}

fn acl_text(acl: &AclObject) -> io::Result<String> {
    let mut length = 0isize;
    // SAFETY: acl is live and length is writable; the result is owned text.
    let text = AclObject::owned(unsafe { acl_to_text(acl.0, &mut length) }.cast())?;
    // SAFETY: acl_to_text returns a NUL-terminated string, still live here.
    Ok(unsafe { CStr::from_ptr(text.0.cast()) }
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
pub(crate) fn test_acl_text(path: &Path) -> String {
    read_acl(&path_cstring(path).unwrap()).unwrap()
}

fn rewrite_acl(current: &str, users: &BTreeSet<u32>) -> String {
    let mut retained = Vec::new();
    let mut inserted = false;
    for line in current.lines().filter(|line| !line.trim().is_empty()) {
        let is_named_user = line
            .strip_prefix("user:")
            .and_then(|rest| rest.split(':').next())
            .is_some_and(|qualifier| !qualifier.is_empty());
        if is_named_user {
            continue;
        }
        if !inserted && line.starts_with("group:") {
            retained.extend(users.iter().map(|uid| format!("user:{uid}:rw-")));
            inserted = true;
        }
        // libacl recalculates the mask from the complete resulting ACL.
        if !line.starts_with("mask:") {
            retained.push(line.to_owned());
        }
    }
    if !inserted {
        retained.extend(users.iter().map(|uid| format!("user:{uid}:rw-")));
    }
    retained.push(String::new());
    retained.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn replaces_named_users_and_preserves_owner_groups_and_other() {
        let current =
            "user::rw-\nuser:1000:r--\ngroup::r--\ngroup:audio:rw-\nmask::rw-\nother::---\n";
        assert_eq!(
            rewrite_acl(current, &BTreeSet::from([1001, 1002])),
            "user::rw-\nuser:1001:rw-\nuser:1002:rw-\ngroup::r--\ngroup:audio:rw-\nother::---\n"
        );
        assert_eq!(
            rewrite_acl(current, &BTreeSet::new()),
            "user::rw-\ngroup::r--\ngroup:audio:rw-\nother::---\n"
        );
    }

    #[test]
    fn libacl_rewrites_a_temporary_file_and_recalculates_the_mask() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o640)).unwrap();
        LibAcl
            .set_users(file.path(), &BTreeSet::from([424_242]))
            .unwrap();
        let path = path_cstring(file.path()).unwrap();
        let granted = read_acl(&path).unwrap();
        assert!(granted.contains("user:424242:rw-"));
        assert!(granted.contains("mask::rw-"));
        assert!(granted.contains("group::r--"));
        LibAcl.set_users(file.path(), &BTreeSet::new()).unwrap();
        let cleared = read_acl(&path).unwrap();
        assert!(!cleared.contains("user:424242:"));
        assert!(cleared.contains("mask::r--"));
        assert!(cleared.contains("user::rw-"));
        assert!(cleared.contains("other::---"));
    }

    #[test]
    fn libacl_preserves_named_groups_while_replacing_named_users() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = path_cstring(file.path()).unwrap();
        let original = CString::new(
            "user::rw-\nuser:424242:r--\ngroup::r--\ngroup:424243:rw-\nmask::rw-\nother::---\n",
        )
        .unwrap();
        // SAFETY: original is NUL terminated; the result is uniquely owned.
        let acl = AclObject::owned(unsafe { acl_from_text(original.as_ptr()) }).unwrap();
        // SAFETY: path and ACL remain live for this call.
        assert_eq!(
            unsafe { acl_set_file(path.as_ptr(), ACL_TYPE_ACCESS, acl.0) },
            0
        );
        LibAcl
            .set_users(file.path(), &BTreeSet::from([424244]))
            .unwrap();
        let replaced = read_acl(&path).unwrap();
        assert!(!replaced.contains("user:424242:"));
        assert!(replaced.contains("user:424244:rw-"));
        assert!(replaced.contains("group:424243:rw-"));
        LibAcl.set_users(file.path(), &BTreeSet::new()).unwrap();
        let cleared = read_acl(&path).unwrap();
        assert!(cleared.contains("group:424243:rw-"));
        assert!(cleared.contains("mask::rw-"));
    }

    #[test]
    fn old_node_cleanup_does_not_modify_a_replacement_inode() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node");
        std::fs::write(&path, b"").unwrap();
        let identity = LibAcl
            .apply_users(&path, &BTreeSet::from([424242]), None)
            .unwrap();
        std::fs::rename(&path, directory.path().join("old")).unwrap();
        std::fs::write(&path, b"").unwrap();
        LibAcl.set_users(&path, &BTreeSet::from([424243])).unwrap();
        assert_eq!(
            LibAcl
                .apply_users(&path, &BTreeSet::new(), identity)
                .unwrap(),
            None
        );
        assert!(
            read_acl(&path_cstring(&path).unwrap())
                .unwrap()
                .contains("user:424243:rw-")
        );
    }

    #[test]
    fn errors_preserve_missing_path_and_invalid_input_information() {
        let directory = tempfile::tempdir().unwrap();
        let error = LibAcl
            .set_users(&directory.path().join("missing"), &BTreeSet::new())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            LibAcl
                .set_users(Path::new("/bad\0path"), &BTreeSet::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
