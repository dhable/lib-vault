//! `FsStore`: objects are regular files under the vault root, one per key (§8).

use crate::fsops::{self, Device};
use crate::journal::{self, Crc32};
use crate::root::Root;
use crate::{FsStoreOptions, PosixError};
use futures::future::{self, BoxFuture};
use lib_vault::io_driver::{AsyncIoDriver, IoDriver};
use lib_vault::{Error, Key, Store};
use rustix::fs::FileType;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;

type ObjectPtr = <AsyncIoDriver as IoDriver>::ObjectPtr;

/// A [`Store`] that keeps each object in a regular file under the vault root.
///
/// **Requires a tokio runtime** (D56): every operation runs its blocking system calls on
/// tokio's blocking pool, so its futures must be polled inside a tokio runtime.
///
/// Behavior, per the `Store` contract and the filesystem-backend decisions:
/// - **Objects are `Vec<u8>`** (D88): `claim.store(key, bytes)` and
///   `claim.load::<Vec<u8>>(key)`. Any other type fails with `Error::WrongType`.
/// - Objects are created by the first `store` (D67) and written **in place** (D19).
/// - With journaling (the default, D63), `store` writes the bytes to `.vault-journal-<name>`
///   first and then rewrites the object; a crash leaves the old object, or a complete journal
///   holding the new one. A leftover journal makes `load`/`store` on that key fail with
///   [`PosixError::InterruptedWrite`] until it's resolved by hand.
/// - Without journaling, `store` truncates the object and writes it directly; a crash can
///   leave a partial object.
/// - A `store` runs as one task on the blocking pool. Dropping its future doesn't cancel the
///   task: the write still runs to completion.
/// - Symlinks, hard-linked files, special files and mount crossings are refused (D17).
/// - Creating a key that differs only by ASCII case from an existing object fails with
///   [`PosixError::CaseCollision`] (D6).
///
/// **Works with any [`Claimer`](lib_vault::Claimer)** (D75): the store never relies on the
/// claimer, which only decides who may claim a key. Use [`FsClaimer`](crate::FsClaimer) for
/// processes on one host, or a distributed claimer when hosts share the storage. Every process
/// using a vault root must use the same kind of claimer, or they won't exclude each other.
#[derive(Clone)]
pub struct FsStore {
    pub(crate) root: Arc<Root>,
}

impl FsStore {
    /// Opens (creating if needed) the vault at `root` and validates it (§9): a real directory,
    /// not a system directory, matching the sharing mode, without extended ACLs (D70), and,
    /// with `strict_modes`, with safe ancestors (D64). Logs a warning on a network filesystem
    /// (D69).
    pub fn new(root: impl AsRef<Path>, options: FsStoreOptions) -> Result<Self, PosixError> {
        let root = Root::open(root.as_ref(), options)?;
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// The vault root's canonical path.
    pub fn root_path(&self) -> &Path {
        &self.root.path
    }

    // Runs `f` on tokio's blocking pool.
    fn blocking<'a, T: Send + 'static>(
        &'a self,
        key: &Key,
        f: impl FnOnce(&Root, &Key) -> Result<T, Error> + Send + 'static,
    ) -> BoxFuture<'a, Result<T, Error>> {
        let root = self.root.clone();
        let key = key.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || f(&root, &key))
                .await
                .map_err(|e| Error::StoreError(Box::new(e)))?
        })
    }
}

impl Store<AsyncIoDriver> for FsStore {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        self.blocking(key, |root, key| {
            list(root, key).map_err(PosixError::into_store_error)
        })
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        self.blocking(key, |root, key| {
            delete(root, key).map_err(PosixError::into_store_error)
        })
    }

    fn load<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectPtr, Error>> {
        // A new `Arc` per load: the caller takes the bytes out of it.
        self.blocking(key, |root, key| {
            load(root, key).map(|data| Arc::new(data) as ObjectPtr)
        })
    }

    fn store<'a>(&'a self, key: &Key, data: ObjectPtr) -> BoxFuture<'a, Result<(), Error>> {
        let Ok(data) = data.downcast::<Vec<u8>>() else {
            return Box::pin(future::ready(Err(Error::WrongType { key: key.clone() })));
        };
        self.blocking(key, move |root, key| {
            store(root, key, &data).map_err(PosixError::into_store_error)
        })
    }
}

// The object's parent directory and file name.
fn split(key: &Key) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = key.components().collect();
    let name = parts.pop().expect("a key has at least one component");
    (parts, name)
}

fn list(root: &Root, key: &Key) -> Result<Vec<Key>, PosixError> {
    let (dirs, name) = split(key);
    let Some(parent) = root.dir(&dirs, false)? else {
        return Ok(Vec::new());
    };
    let path = root.path_of(&dirs, name);
    let is_dir = fsops::stat_at(&parent, name, &path)?
        .is_some_and(|s| fsops::file_type(&s) == FileType::Directory);
    if !is_dir {
        return Ok(Vec::new());
    }
    let dir = fsops::open_dir(&parent, name, root.dev, &path)?.expect("checked above");
    let mut out = Vec::new();
    collect(root, &dir, &path, None, &mut out)?;
    Ok(out)
}

// Every regular file beneath `dir`, as keys relative to the listed key. Reserved files,
// names that aren't valid keys, symlinks and special files are skipped.
fn collect(
    root: &Root,
    dir: &OwnedFd,
    path: &Path,
    prefix: Option<&Key>,
    out: &mut Vec<Key>,
) -> Result<(), PosixError> {
    for name in entries(dir, path)? {
        let Ok(component) = Key::new(&name) else {
            continue;
        };
        let Some(key) = (match prefix {
            Some(p) => p.join(&component).ok(),
            None => Some(component),
        }) else {
            continue;
        };
        let entry_path = path.join(&name);
        let Some(stat) = fsops::stat_at(dir, &name, &entry_path)? else {
            continue;
        };
        match fsops::file_type(&stat) {
            FileType::RegularFile => out.push(key),
            FileType::Directory if fsops::dev_of(&stat) == root.dev => {
                if let Some(sub) = fsops::open_dir(dir, &name, root.dev, &entry_path)? {
                    collect(root, &sub, &entry_path, Some(&key), out)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// Names in `dir`, without `.`, `..` and the backend's reserved files.
fn entries(dir: &OwnedFd, path: &Path) -> Result<Vec<String>, PosixError> {
    let reader = rustix::fs::Dir::read_from(dir).map_err(|e| PosixError::io(path, e))?;
    let mut names = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|e| PosixError::io(path, e))?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if name == "." || name == ".." || fsops::is_reserved(name) {
            continue;
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn delete(root: &Root, key: &Key) -> Result<(), PosixError> {
    let (dirs, name) = split(key);
    let Some(parent) = root.dir(&dirs, false)? else {
        return Ok(());
    };
    let path = root.path_of(&dirs, name);
    match fsops::stat_at(&parent, name, &path)? {
        // Only regular files are objects; the journal (if any) is left alone (D63).
        Some(stat) if fsops::file_type(&stat) == FileType::RegularFile => {
            fsops::unlink(&parent, name, &path)
        }
        _ => Ok(()),
    }
}

fn check_journal(
    root: &Root,
    parent: &OwnedFd,
    dirs: &[&str],
    name: &str,
    key: &Key,
) -> Result<(), PosixError> {
    let journal = fsops::journal_name(name);
    let path = root.path_of(dirs, &journal);
    if fsops::stat_at(parent, &journal, &path)?.is_some() {
        return Err(PosixError::InterruptedWrite {
            key: key.clone(),
            journal: path,
        });
    }
    Ok(())
}

fn load(root: &Root, key: &Key) -> Result<Vec<u8>, Error> {
    let (dirs, name) = split(key);
    let not_found = || Error::NotFound { key: key.clone() };
    let parent = root
        .dir(&dirs, false)
        .map_err(PosixError::into_store_error)?
        .ok_or_else(not_found)?;
    check_journal(root, &parent, &dirs, name, key).map_err(PosixError::into_store_error)?;
    let path = root.path_of(&dirs, name);
    let mut file = fsops::open_file(&parent, name, false, root.dev, &path)
        .map_err(PosixError::into_store_error)?
        .ok_or_else(not_found)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| PosixError::io(&path, e).into_store_error())?;
    Ok(data)
}

fn store(root: &Root, key: &Key, data: &[u8]) -> Result<(), PosixError> {
    let (dirs, name) = split(key);
    let parent = root.dir(&dirs, true)?.expect("created");
    check_journal(root, &parent, &dirs, name, key)?;
    let path = root.path_of(&dirs, name);

    // D6: creating `name` must not collide by case with an existing object.
    let parent_path = root.path_of(&dirs, "");
    for existing in entries(&parent, &parent_path)? {
        if existing != name && existing.eq_ignore_ascii_case(name) {
            return Err(PosixError::CaseCollision {
                key: key.clone(),
                existing,
            });
        }
    }
    // An existing object must pass the same handle checks as a load (D17: no symlink, special
    // file, hard link or mount crossing), even though the journal delays touching it.
    if fsops::stat_at(&parent, name, &path)?.is_some() {
        fsops::open_file(&parent, name, true, root.dev, &path)?;
    }

    if !root.options.journaling {
        // Straight to the object: a crash can leave it partly written.
        let mut object = open_or_create(&parent, name, root.dev, root.options.file_mode(), &path)?;
        object.set_len(0).map_err(|e| PosixError::io(&path, e))?;
        object
            .write_all(data)
            .map_err(|e| PosixError::io(&path, e))?;
        return fsops::sync(&object, &path);
    }

    let journal = fsops::journal_name(name);
    let journal_path = root.path_of(&dirs, &journal);
    let mut file = fsops::create_file(
        &parent,
        &journal,
        true,
        root.options.file_mode(),
        &journal_path,
    )?
    .ok_or_else(|| PosixError::InterruptedWrite {
        key: key.clone(),
        journal: journal_path.clone(),
    })?;

    // Until the commit starts the object is untouched, so a journal that can't be written is
    // simply removed (D63).
    let written = file
        .write_all(&journal::placeholder())
        .and_then(|()| file.write_all(data));
    if let Err(e) = written {
        let _ = fsops::unlink(&parent, &journal, &journal_path);
        return Err(PosixError::io(&journal_path, e));
    }

    // The commit (D43, D63). From here on a failure may leave the object partly rewritten:
    // the journal stays (it's the only complete copy), and the next `load`/`store` reports it.
    // 1. Complete and persist the journal.
    let mut crc = Crc32::new();
    crc.update(data);
    journal::complete(&file, data.len() as u64, crc.finish())
        .map_err(|e| PosixError::io(&journal_path, e))?;
    fsops::sync(&file, &journal_path)?;
    fsops::sync(parent.as_fd(), &path)?;
    // 2. Rewrite the object in place (created now if it didn't exist, D67).
    let mut object = open_or_create(&parent, name, root.dev, root.options.file_mode(), &path)?;
    object.set_len(0).map_err(|e| PosixError::io(&path, e))?;
    object
        .write_all(data)
        .map_err(|e| PosixError::io(&path, e))?;
    fsops::sync(&object, &path)?;
    // 3. Drop the journal.
    fsops::unlink(&parent, &journal, &journal_path)?;
    fsops::sync(parent.as_fd(), &path)
}

// Opens the object for writing, creating it (with `mode`) if missing.
fn open_or_create(
    dir: &OwnedFd,
    name: &str,
    dev: Device,
    mode: u32,
    path: &Path,
) -> Result<File, PosixError> {
    loop {
        if let Some(file) = fsops::open_file(dir, name, true, dev, path)? {
            return Ok(file);
        }
        if let Some(file) = fsops::create_file(dir, name, true, mode, path)? {
            return Ok(file);
        }
    }
}
