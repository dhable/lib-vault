//! `FsClaimer`: the "lock-file v1" protocol (D16, D67).
//!
//! Each key component `c` is represented by a hidden lock file `.vault-lock-c` in its parent
//! directory. Claiming `c1/…/cn` takes shared `flock`s on the lock files of `c1 … c(n-1)` and
//! an exclusive one on `cn`'s, top-down, verifying after each lock that the name still refers
//! to the locked inode. Intermediate directories are created as needed; the object itself is
//! only created on write.

use crate::PosixError;
use crate::fsops;
use crate::root::Root;
use futures::future::BoxFuture;
use lib_vault::{ClaimLock, Claimer, Error, Key};
use std::fs::{File, TryLockError};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::Arc;

/// The [`Claimer`] paired with [`FsStore`](crate::FsStore) (D45): it locks lock files inside
/// the store's own directory tree, so it only works with the store it came from
/// ([`FsStore::claimer`](crate::FsStore::claimer)).
///
/// Exclusion holds across threads, `Vault`s and processes that use the same vault root,
/// because `flock` locks belong to open file descriptions, not processes.
#[derive(Clone)]
pub struct FsClaimer {
    pub(crate) root: Arc<Root>,
}

impl Claimer for FsClaimer {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
        let root = self.root.clone();
        let key = key.clone();
        Box::pin(async move {
            // Dropping this future before the task finishes drops the returned lock, which
            // closes its descriptors and so releases anything acquired.
            tokio::task::spawn_blocking(move || lock(&root, &key))
                .await
                .map_err(|e| Error::ClaimError(Box::new(e)))?
                .map(|lock| Box::new(lock) as Box<dyn ClaimLock>)
        })
    }
}

/// The locks one claim holds: shared on ancestors, exclusive on the target (last).
struct FsLock {
    held: Vec<File>,
    target_dir: OwnedFd,
    target_lock: String,
    target_path: std::path::PathBuf,
}

impl ClaimLock for FsLock {
    fn release(self: Box<Self>) -> Result<(), Error> {
        // Unlink the target's lock file while still holding it exclusively: nobody else can
        // hold it now, and anyone who opened it just before fails verify-after-lock (nlink 0)
        // and retries with a fresh file. Closing the descriptors then releases every lock.
        fsops::unlink(&self.target_dir, &self.target_lock, &self.target_path)
            .map_err(PosixError::into_claim_error)?;
        drop(self.held);
        Ok(())
    }
}

enum LockError {
    Conflict,
    Other(PosixError),
}

impl From<PosixError> for LockError {
    fn from(e: PosixError) -> Self {
        LockError::Other(e)
    }
}

fn lock(root: &Root, key: &Key) -> Result<FsLock, Error> {
    let parts: Vec<&str> = key.components().collect();
    let last = parts.len() - 1;
    let mut dir = root
        .dir(&[], false)
        .map_err(PosixError::into_claim_error)?
        .expect("the root directory exists");
    let mut held = Vec::with_capacity(parts.len());

    for (i, part) in parts.iter().enumerate() {
        let exclusive = i == last;
        let lock_name = fsops::lock_name(part);
        let lock_path = root.path_of(&parts[..i], &lock_name);
        match lock_one(root, &dir, &lock_name, exclusive, key, &lock_path) {
            Ok(file) => held.push(file),
            Err(LockError::Conflict) => return Err(Error::AlreadyClaimed { key: key.clone() }),
            Err(LockError::Other(e)) => return Err(e.into_claim_error()),
        }
        if exclusive {
            return Ok(FsLock {
                held,
                target_dir: dir,
                target_lock: lock_name,
                target_path: lock_path,
            });
        }
        // A lock file needs its parent directory, so intermediate directories are created at
        // claim time (D67). They aren't objects.
        let dir_path = root.path_of(&parts[..i], part);
        dir = fsops::open_or_create_dir(&dir, part, root.options.dir_mode(), root.dev, &dir_path)
            .map_err(PosixError::into_claim_error)?;
    }
    unreachable!("a key has at least one component")
}

// Opens (creating if needed) and locks one lock file, retrying if verify-after-lock shows the
// name now refers to a different (or deleted) inode (§8.3 step 3, D66).
fn lock_one(
    root: &Root,
    dir: &OwnedFd,
    name: &str,
    exclusive: bool,
    key: &Key,
    path: &Path,
) -> Result<File, LockError> {
    let mut attempts = 0;
    loop {
        let file = match fsops::open_file(dir, name, false, root.dev, path)? {
            Some(file) => file,
            None => match fsops::create_file(dir, name, false, root.options.file_mode(), path)? {
                Some(file) => file,
                None => continue, // created concurrently; open it on the next pass
            },
        };
        let locked = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match locked {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(LockError::Conflict),
            Err(TryLockError::Error(e)) => return Err(PosixError::io(path, e).into()),
        }
        if verify(dir, name, &file, path)? {
            return Ok(file);
        }
        drop(file);
        attempts += 1;
        if attempts > root.options.max_verify_retries {
            return Err(PosixError::VerifyRetriesExhausted {
                key: key.clone(),
                path: path.to_path_buf(),
                attempts,
            }
            .into());
        }
    }
}

// The locked descriptor must still be the file at `name`, and not unlinked.
fn verify(dir: &OwnedFd, name: &str, file: &File, path: &Path) -> Result<bool, PosixError> {
    let locked = rustix::fs::fstat(file).map_err(|e| PosixError::io(path, e))?;
    let current = fsops::stat_at(dir, name, path)?;
    Ok(locked.st_nlink > 0 && current.is_some_and(|c| fsops::same_inode(&c, &locked)))
}
