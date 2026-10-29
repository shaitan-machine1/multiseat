//! Linux access-ACL rewriting, adapted from aclad's `src/acl.rs` at
//! b4347297c895c47074555ea8eb0a9432e8a3cb0e (GPL-3.0-only).
//! Static-node discovery belongs to seat-udev, not this backend.
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::path::Path;

const ACL_TYPE_ACCESS: c_int = 0x8000;
type Acl = *mut c_void;

#[link(name = "acl")]
unsafe extern "C" {
    fn acl_get_file(path: *const c_char, acl_type: c_int) -> Acl;
    fn acl_from_text(text: *const c_char) -> Acl;
    fn acl_to_text(acl: Acl, length: *mut isize) -> *mut c_char;
    fn acl_calc_mask(acl: *mut Acl) -> c_int;
    fn acl_set_file(path: *const c_char, acl_type: c_int, acl: Acl) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

/// Internal daemon test seam. Production always uses Linux libacl.
pub trait AclOps {
    fn set_users(&self, path: &Path, users: &BTreeSet<u32>) -> io::Result<()>;
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
        let path = path_cstring(path)?;
        let current = read_acl(&path)?;
        let rewritten = CString::new(rewrite_acl(&current, users))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // SAFETY: rewritten is NUL terminated; libacl returns an owned ACL.
        let mut acl = AclObject::owned(unsafe { acl_from_text(rewritten.as_ptr()) })?;
        // SAFETY: acl is owned and writable; libacl may replace its allocation.
        if unsafe { acl_calc_mask(&mut acl.0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: path and acl are live for this call.
        if unsafe { acl_set_file(path.as_ptr(), ACL_TYPE_ACCESS, acl.0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn read_acl(path: &CStr) -> io::Result<String> {
    // SAFETY: path is NUL terminated; returned ACL is owned by this wrapper.
    let acl = AclObject::owned(unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_ACCESS) })?;
    let mut length = 0isize;
    // SAFETY: acl is live and length is writable; the result is owned text.
    let text = AclObject::owned(unsafe { acl_to_text(acl.0, &mut length) }.cast())?;
    // SAFETY: acl_to_text returns a NUL-terminated string, still live here.
    Ok(unsafe { CStr::from_ptr(text.0.cast()) }
        .to_string_lossy()
        .into_owned())
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
