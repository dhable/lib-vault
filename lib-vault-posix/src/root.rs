//! The vault root: opened once at construction, validated (§9), then used as the base handle
//! for every operation (§8.1), so later renames of the path have no effect.

use crate::fsops::{self, Device};
use crate::{FsStoreOptions, PosixError, Sharing};
use rustix::fs::{AtFlags, CWD, FileType, Gid, Mode, OFlags};
use rustix::io::Errno;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Directories that are never accepted as a vault root.
const SYSTEM_DIRS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/var",
    "/Applications",
    "/Library",
    "/System",
    "/Users",
    "/Volumes",
    "/private",
    "/private/etc",
    "/private/tmp",
    "/private/var",
];

pub(crate) struct Root {
    pub(crate) fd: OwnedFd,
    pub(crate) dev: Device,
    pub(crate) path: PathBuf,
    pub(crate) options: FsStoreOptions,
}

impl Root {
    pub(crate) fn open(path: &Path, options: FsStoreOptions) -> Result<Self, PosixError> {
        let invalid = |reason: &str| PosixError::InvalidRoot {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        };

        // Resolve the parent (ancestors may legitimately be symlinks, e.g. macOS `/var`), but
        // never the root itself: it's opened no-follow below.
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| PosixError::io(path, e))?
                .join(path)
        };
        let name = absolute
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| invalid("must name a directory below `/`"))?
            .to_owned();
        let parent = absolute
            .parent()
            .ok_or_else(|| invalid("must not be `/`"))?;
        let parent = std::fs::canonicalize(parent).map_err(|e| PosixError::io(parent, e))?;
        let path = parent.join(&name);
        if SYSTEM_DIRS.iter().any(|d| Path::new(d) == path) {
            return Err(PosixError::InvalidRoot {
                path,
                reason: "system directories can't be vault roots".into(),
            });
        }

        let parent_fd = rustix::fs::openat(
            CWD,
            &parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| PosixError::io(&parent, e))?;

        // Create the root if needed; `new` is idempotent across apps (§9).
        let created = match rustix::fs::mkdirat(&parent_fd, name.as_str(), fsops::mode_bits(0o700))
        {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(e) => return Err(PosixError::io(&path, e)),
        };
        let fd = match rustix::fs::openat(
            &parent_fd,
            name.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP) => return Err(invalid("is a symlink")),
            Err(Errno::NOTDIR) => return Err(invalid("is not a directory")),
            Err(e) => return Err(PosixError::io(&path, e)),
        };
        if created {
            apply_sharing(&fd, options.sharing, &path)?;
        }
        // ACLs before the mode: on Linux an ACL entry also sets the group bits to the ACL
        // mask (a `0700` root reads as `0750`), so checking the mode first would report a
        // wrong mode instead of the ACL that caused it.
        if crate::acl::has_extended_acl(fd.as_fd()).map_err(|e| PosixError::io(&path, e))? {
            return Err(PosixError::ExtendedAcl { path });
        }
        check_sharing(&fd, options.sharing, &path)?;
        warn_if_not_local(&fd, &path);
        if options.strict_modes {
            check_strict_modes(&parent)?;
        }

        let dev = fsops::dev(&fd, &path)?;
        Ok(Root {
            fd,
            dev,
            path,
            options,
        })
    }

    /// The display path of `key`'s directory components plus `name`, for error messages.
    pub(crate) fn path_of(&self, dirs: &[&str], name: &str) -> PathBuf {
        let mut path = self.path.clone();
        path.extend(dirs);
        path.push(name);
        path
    }

    /// Opens the directory at `dirs` below the root (`[]` is the root itself).
    pub(crate) fn dir(&self, dirs: &[&str], create: bool) -> Result<Option<OwnedFd>, PosixError> {
        let mut current = rustix::io::dup(&self.fd).map_err(|e| PosixError::io(&self.path, e))?;
        let mut path = self.path.clone();
        for name in dirs {
            path.push(name);
            current = if create {
                fsops::open_or_create_dir(&current, name, self.options.dir_mode(), self.dev, &path)?
            } else {
                match fsops::open_dir(&current, name, self.dev, &path)? {
                    Some(fd) => fd,
                    None => return Ok(None),
                }
            };
        }
        Ok(Some(current))
    }
}

fn apply_sharing(fd: &OwnedFd, sharing: Sharing, path: &Path) -> Result<(), PosixError> {
    if let Sharing::Group(gid) = sharing {
        rustix::fs::fchown(fd, None, Some(Gid::from_raw(gid)))
            .map_err(|e| PosixError::io(path, e))?;
    }
    let mode = match sharing {
        Sharing::Private => 0o700,
        Sharing::Group(_) => 0o2770,
    };
    rustix::fs::fchmod(fd, fsops::mode_bits(mode)).map_err(|e| PosixError::io(path, e))
}

// The root must match the requested sharing mode exactly; nothing world-accessible (§9).
fn check_sharing(fd: &OwnedFd, sharing: Sharing, path: &Path) -> Result<(), PosixError> {
    let stat = rustix::fs::fstat(fd).map_err(|e| PosixError::io(path, e))?;
    let perm = stat.st_mode as u32 & 0o7777;
    let reason = match sharing {
        Sharing::Private if stat.st_uid != rustix::process::geteuid().as_raw() => Some(format!(
            "private mode requires the root to be owned by uid {}",
            rustix::process::geteuid().as_raw()
        )),
        Sharing::Private if perm != 0o700 => {
            Some(format!("private mode requires mode 0700, found {perm:04o}"))
        }
        Sharing::Group(gid) if stat.st_gid != gid => Some(format!(
            "group mode requires group {gid}, found {}",
            stat.st_gid
        )),
        Sharing::Group(_) if perm != 0o2770 => {
            Some(format!("group mode requires mode 2770, found {perm:04o}"))
        }
        _ => None,
    };
    match reason {
        Some(reason) => Err(PosixError::InvalidRoot {
            path: path.to_path_buf(),
            reason,
        }),
        None => Ok(()),
    }
}

// sshd-style `StrictModes` (D64): every ancestor must be owned by root or the effective uid
// and not writable by group or others, unless it's root-owned with the sticky bit.
fn check_strict_modes(parent: &Path) -> Result<(), PosixError> {
    let euid = rustix::process::geteuid().as_raw();
    for dir in parent.ancestors() {
        let stat = rustix::fs::statat(CWD, dir, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| PosixError::io(dir, e))?;
        let violation = |reason: &str| PosixError::StrictModes {
            path: dir.to_path_buf(),
            reason: reason.to_owned(),
        };
        if fsops::file_type(&stat) != FileType::Directory {
            return Err(violation("not a directory"));
        }
        if stat.st_uid != 0 && stat.st_uid != euid {
            return Err(violation("owned by another user"));
        }
        let mode = stat.st_mode as u32;
        let sticky_root = mode & 0o1000 != 0 && stat.st_uid == 0;
        if mode & 0o022 != 0 && !sticky_root {
            return Err(violation("writable by group or others"));
        }
    }
    Ok(())
}

// Network filesystems are allowed with a warning (D69): `flock` over NFS/SMB may not exclude
// processes on other hosts.
fn warn_if_not_local(fd: &OwnedFd, path: &Path) {
    let Ok(stat) = rustix::fs::fstatfs(fd) else {
        return;
    };
    if !is_local(&stat) {
        tracing::warn!(
            path = %path.display(),
            "vault root is not on a local filesystem; locks may not exclude other hosts"
        );
    }
}

#[cfg(target_os = "linux")]
fn is_local(stat: &rustix::fs::StatFs) -> bool {
    const REMOTE: &[i64] = &[
        0x6969,      // NFS
        0x517B,      // SMB
        0xFF53_4D42, // CIFS
        0xFE53_4D42, // SMB2
        0x7375_7245, // CODA
        0x5346_414F, // AFS
        0x6573_5546, // FUSE (sshfs and friends)
        0x0102_1997, // 9P
        0x00C3_6400, // CEPH
    ];
    #[allow(clippy::unnecessary_cast)]
    !REMOTE.contains(&(stat.f_type as i64))
}

#[cfg(target_vendor = "apple")]
fn is_local(stat: &rustix::fs::StatFs) -> bool {
    const MNT_LOCAL: u32 = 0x0000_1000;
    stat.f_flags & MNT_LOCAL != 0
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn is_local(_stat: &rustix::fs::StatFs) -> bool {
    true
}
