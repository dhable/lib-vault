//! Extended-ACL detection for the vault root (D70).

use std::io;
use std::os::fd::BorrowedFd;

/// True if `dir` carries an extended (POSIX or NFSv4-style) ACL.
#[cfg(target_os = "linux")]
pub(crate) fn has_extended_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    use rustix::io::Errno;
    for name in ["system.posix_acl_access", "system.posix_acl_default"] {
        match rustix::fs::fgetxattr(dir, name, &mut [] as &mut [u8]) {
            Ok(_) => return Ok(true),
            Err(Errno::NODATA | Errno::NOTSUP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

/// True if `dir` carries an extended ACL. macOS doesn't expose ACLs through extended
/// attributes, and neither `rustix` nor `libc` binds the ACL API, so this declares the three
/// functions it needs (an exception to D52's "no libc-level `unsafe`").
#[cfg(target_vendor = "apple")]
pub(crate) fn has_extended_acl(dir: BorrowedFd<'_>) -> io::Result<bool> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_free(obj: *mut c_void) -> c_int;
    }

    // SAFETY: `dir` is a valid open descriptor for the duration of the call; the returned
    // ACL is checked for null, only passed to `acl_get_entry`, and freed exactly once.
    unsafe {
        let acl = acl_get_fd_np(dir.as_raw_fd(), ACL_TYPE_EXTENDED);
        if acl.is_null() {
            let err = io::Error::last_os_error();
            // ENOENT: no ACL. ENOTSUP/EOPNOTSUPP: the filesystem has no ACLs.
            return match err.raw_os_error() {
                Some(2) | Some(45) | Some(102) => Ok(false),
                _ => Err(err),
            };
        }
        let mut entry: *mut c_void = std::ptr::null_mut();
        let has_entry = acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) == 0;
        acl_free(acl);
        Ok(has_entry)
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub(crate) fn has_extended_acl(_dir: BorrowedFd<'_>) -> io::Result<bool> {
    Ok(false)
}
