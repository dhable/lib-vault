//! Handle-relative file operations (§8.1, §8.2): every open is relative to a held directory
//! handle, never follows symlinks, and is checked on the handle (`fstat`), not the path.

use crate::PosixError;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

/// Prefix reserved for the backend's own files (lock files, journals). `Key` rejects
/// components starting with `.vault` (D6), so these never collide with objects.
pub(crate) const RESERVED_PREFIX: &str = ".vault";
const LOCK_PREFIX: &str = ".vault-lock-";
const JOURNAL_PREFIX: &str = ".vault-journal-";
const NAME_MAX: usize = 255;

/// The lock file for key component `name` (D67).
pub(crate) fn lock_name(name: &str) -> String {
    reserved_name(LOCK_PREFIX, name)
}

/// The journal for the object `name` (D63).
pub(crate) fn journal_name(name: &str) -> String {
    reserved_name(JOURNAL_PREFIX, name)
}

// `<prefix><name>`, or `<prefix>#<hash>` when that would exceed the filesystem's 255-byte name
// limit (a key component may be 255 bytes on its own). The hash is of the ASCII-lowercased
// name, so names that alias on a case-insensitive volume still share one file; two names
// sharing a hash only over-exclude, never under-exclude.
fn reserved_name(prefix: &str, name: &str) -> String {
    if prefix.len() + name.len() <= NAME_MAX {
        return format!("{prefix}{name}");
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for b in name.bytes().map(|b| b.to_ascii_lowercase()) {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{prefix}#{hash:016x}")
}

/// `mode` as a `rustix` `Mode` (`mode_t` is `u16` on macOS, `u32` on Linux).
pub(crate) fn mode_bits(mode: u32) -> Mode {
    #[allow(clippy::unnecessary_cast)]
    Mode::from_raw_mode(mode as rustix::fs::RawMode)
}

pub(crate) fn is_reserved(name: &str) -> bool {
    name.starts_with(RESERVED_PREFIX)
}

/// The device every vault file must be on (no mount crossings, §8.2).
pub(crate) type Device = u64;

fn dev_of(stat: &Stat) -> Device {
    #[allow(clippy::useless_conversion)]
    u64::from(stat.st_dev as u64)
}

/// Opens directory `name` in `dir` without following symlinks, checking it's a directory on
/// `dev`. `Ok(None)` if it doesn't exist.
pub(crate) fn open_dir(
    dir: impl AsFd,
    name: &str,
    dev: Device,
    path: &Path,
) -> Result<Option<OwnedFd>, PosixError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
        Ok(fd) => {
            check(&fd, FileType::Directory, dev, path)?;
            Ok(Some(fd))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::NOTDIR | Errno::LOOP) => Err(unsupported(path, "not a directory")),
        Err(e) => Err(PosixError::io(path, e)),
    }
}

/// Like [`open_dir`], creating the directory (with `mode`, set explicitly) if it's missing.
pub(crate) fn open_or_create_dir(
    dir: impl AsFd,
    name: &str,
    mode: u32,
    dev: Device,
    path: &Path,
) -> Result<OwnedFd, PosixError> {
    if let Some(fd) = open_dir(&dir, name, dev, path)? {
        return Ok(fd);
    }
    match rustix::fs::mkdirat(&dir, name, mode_bits(mode)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(PosixError::io(path, e)),
    }
    let fd = open_dir(&dir, name, dev, path)?
        .ok_or_else(|| PosixError::io(path, std::io::Error::from(std::io::ErrorKind::NotFound)))?;
    set_mode_if_owner(&fd, mode, path)?;
    Ok(fd)
}

/// Opens regular file `name` in `dir` without following symlinks (and without blocking on a
/// FIFO), checking it's a single-linked regular file on `dev`. `Ok(None)` if it doesn't exist.
pub(crate) fn open_file(
    dir: impl AsFd,
    name: &str,
    write: bool,
    dev: Device,
    path: &Path,
) -> Result<Option<File>, PosixError> {
    let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
    let flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
        Ok(fd) => {
            check(&fd, FileType::RegularFile, dev, path)?;
            clear_nonblock(&fd, path)?;
            Ok(Some(File::from(fd)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(unsupported(path, "symlink")),
        Err(Errno::ISDIR) => Err(unsupported(path, "directory, not a file")),
        Err(e) => Err(PosixError::io(path, e)),
    }
}

/// Creates regular file `name` in `dir` exclusively, with `mode` set explicitly.
/// `Ok(None)` if it already exists.
pub(crate) fn create_file(
    dir: impl AsFd,
    name: &str,
    write: bool,
    mode: u32,
    path: &Path,
) -> Result<Option<File>, PosixError> {
    let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
    let flags = access | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(&dir, name, flags, mode_bits(mode)) {
        Ok(fd) => {
            rustix::fs::fchmod(&fd, mode_bits(mode)).map_err(|e| PosixError::io(path, e))?;
            Ok(Some(File::from(fd)))
        }
        Err(Errno::EXIST) => Ok(None),
        Err(e) => Err(PosixError::io(path, e)),
    }
}

/// `lstat` relative to `dir`. `Ok(None)` if `name` doesn't exist.
pub(crate) fn stat_at(dir: impl AsFd, name: &str, path: &Path) -> Result<Option<Stat>, PosixError> {
    match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(PosixError::io(path, e)),
    }
}

/// Removes file `name` from `dir`; a missing file is fine.
pub(crate) fn unlink(dir: impl AsFd, name: &str, path: &Path) -> Result<(), PosixError> {
    match rustix::fs::unlinkat(&dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(PosixError::io(path, e)),
    }
}

/// Flushes `fd` to stable storage: `F_FULLFSYNC` on macOS (a plain `fsync` there doesn't
/// guarantee the data reached the disk), `fsync` elsewhere.
pub(crate) fn sync(fd: impl AsFd, path: &Path) -> Result<(), PosixError> {
    #[cfg(target_vendor = "apple")]
    let result = rustix::fs::fcntl_fullfsync(&fd);
    #[cfg(not(target_vendor = "apple"))]
    let result = rustix::fs::fsync(&fd);
    result.map_err(|e| PosixError::io(path, e))
}

pub(crate) fn dev(fd: impl AsFd, path: &Path) -> Result<Device, PosixError> {
    let stat = rustix::fs::fstat(&fd).map_err(|e| PosixError::io(path, e))?;
    Ok(dev_of(&stat))
}

pub(crate) fn file_type(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode as _)
}

pub(crate) fn same_inode(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

// `fstat` checks on an opened handle (D17).
fn check(fd: impl AsFd, kind: FileType, dev: Device, path: &Path) -> Result<(), PosixError> {
    let stat = rustix::fs::fstat(&fd).map_err(|e| PosixError::io(path, e))?;
    let actual = file_type(&stat);
    if actual != kind {
        let reason = match actual {
            FileType::Symlink => "symlink",
            FileType::Directory => "directory, not a file",
            FileType::RegularFile => "file, not a directory",
            _ => "special file (FIFO, socket or device)",
        };
        return Err(unsupported(path, reason));
    }
    if dev_of(&stat) != dev {
        return Err(unsupported(path, "on a different filesystem (mount point)"));
    }
    if kind == FileType::RegularFile && stat.st_nlink > 1 {
        return Err(unsupported(path, "hard-linked file"));
    }
    Ok(())
}

fn clear_nonblock(fd: impl AsFd, path: &Path) -> Result<(), PosixError> {
    let flags = rustix::fs::fcntl_getfl(&fd).map_err(|e| PosixError::io(path, e))?;
    rustix::fs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK).map_err(|e| PosixError::io(path, e))
}

// Sets `mode` explicitly (independent of umask) on a directory we may have just created.
// Only the owner can chmod; a directory created concurrently by another vault user already
// has its mode set by them.
fn set_mode_if_owner(fd: impl AsFd, mode: u32, path: &Path) -> Result<(), PosixError> {
    let stat = rustix::fs::fstat(&fd).map_err(|e| PosixError::io(path, e))?;
    if stat.st_uid == rustix::process::geteuid().as_raw() {
        rustix::fs::fchmod(&fd, mode_bits(mode)).map_err(|e| PosixError::io(path, e))?;
    }
    Ok(())
}

pub(crate) fn unsupported(path: &Path, reason: &'static str) -> PosixError {
    PosixError::Unsupported {
        path: path.to_path_buf(),
        reason,
    }
}
