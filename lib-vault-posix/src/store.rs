//! `FsStore`: objects are regular files under the vault root, one per key (§8).

use crate::fsops;
use crate::journal::{self, Crc32};
use crate::root::Root;
use crate::{FsStoreOptions, PosixError};
use futures::future::BoxFuture;
use futures::io::AsyncWrite;
use lib_vault::io_driver::AsyncIoDriver;
use lib_vault::io_driver::async_io::{ObjectReader, ObjectWriter};
use lib_vault::{Error, Key, Store};
use rustix::fs::FileType;
use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use tokio::task::JoinHandle;
use tokio_util::compat::TokioAsyncReadCompatExt;

/// A [`Store`] that keeps each object in a regular file under the vault root.
///
/// **Requires a tokio runtime** (D56): every operation runs its blocking system calls on
/// tokio's blocking pool, so its futures (and readers and writers) must be polled inside a
/// tokio runtime.
///
/// Behavior, per the `Store` contract and the filesystem-backend decisions:
/// - Objects are created on the first write (D67) and written **in place** (D19).
/// - With journaling (the default, D63), writes go to `.vault-journal-<name>` first and the
///   object is rewritten only on `close()`, the commit point; a crash leaves the old object
///   and a journal. A leftover journal makes `open_read`/`open_write` on that key fail with
///   [`PosixError::InterruptedWrite`] until it's resolved by hand.
/// - Without journaling, `open_write` truncates the object and writes it directly; a crash can
///   leave a partial object.
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

    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
        let open = self.blocking(key, open_read);
        Box::pin(async move {
            let file = open.await?;
            let reader = tokio::fs::File::from_std(file).compat();
            Ok(Box::pin(reader) as ObjectReader<'a>)
        })
    }

    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
        let open = self.blocking(key, open_write);
        Box::pin(async move { Ok(Box::pin(open.await?) as ObjectWriter<'a>) })
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
            FileType::Directory if stat.st_dev as u64 == root.dev => {
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

fn open_read(root: &Root, key: &Key) -> Result<File, Error> {
    let (dirs, name) = split(key);
    let not_found = || Error::NotFound { key: key.clone() };
    let parent = root
        .dir(&dirs, false)
        .map_err(PosixError::into_store_error)?
        .ok_or_else(not_found)?;
    check_journal(root, &parent, &dirs, name, key).map_err(PosixError::into_store_error)?;
    let path = root.path_of(&dirs, name);
    fsops::open_file(&parent, name, false, root.dev, &path)
        .map_err(PosixError::into_store_error)?
        .ok_or_else(not_found)
}

fn open_write(root: &Root, key: &Key) -> Result<FsWriter, Error> {
    open_writer(root, key).map_err(PosixError::into_store_error)
}

fn open_writer(root: &Root, key: &Key) -> Result<FsWriter, PosixError> {
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
    // An existing object must pass the same handle checks as a read (D17: no symlink, special
    // file, hard link or mount crossing), even though the journal delays touching it.
    if fsops::stat_at(&parent, name, &path)?.is_some() {
        fsops::open_file(&parent, name, true, root.dev, &path)?;
    }

    let target = if root.options.journaling {
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
        file.write_all(&journal::placeholder())
            .map_err(|e| PosixError::io(&journal_path, e))?;
        Target::Journal {
            name: journal,
            path: journal_path,
        }
        .with(file)
    } else {
        let file = open_or_create_object(root, &parent, name, &path)?;
        file.set_len(0).map_err(|e| PosixError::io(&path, e))?;
        (Target::Direct, file)
    };

    let (target, file) = target;
    Ok(FsWriter {
        ctx: Arc::new(WriteCtx {
            dir: parent,
            name: name.to_owned(),
            path,
            dev: root.dev,
            file_mode: root.options.file_mode(),
            target,
        }),
        state: State::Idle(Some(file)),
        buf: Vec::new(),
        len: 0,
        crc: Crc32::new(),
        commit_started: false,
    })
}

// Opens the object for writing, creating it (with the vault's file mode) if missing.
fn open_or_create_object(
    root: &Root,
    dir: &OwnedFd,
    name: &str,
    path: &Path,
) -> Result<File, PosixError> {
    open_or_create(dir, name, root.dev, root.options.file_mode(), path)
}

fn open_or_create(
    dir: &OwnedFd,
    name: &str,
    dev: u64,
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

enum Target {
    /// Writing `.vault-journal-<name>`; the object is rewritten on `close()`.
    Journal { name: String, path: PathBuf },
    /// Writing the object itself.
    Direct,
}

impl Target {
    fn with(self, file: File) -> (Target, File) {
        (self, file)
    }
}

struct WriteCtx {
    dir: OwnedFd,
    name: String,
    path: PathBuf,
    dev: u64,
    file_mode: u32,
    target: Target,
}

enum State {
    Idle(Option<File>),
    Flushing(JoinHandle<(File, io::Result<()>)>),
    Committing(JoinHandle<Result<(), PosixError>>),
    Done,
}

/// Buffers writes and hands them to tokio's blocking pool; commits on `close()`.
struct FsWriter {
    ctx: Arc<WriteCtx>,
    state: State,
    buf: Vec<u8>,
    len: u64,
    crc: Crc32,
    /// Set once the commit starts: from then on the object may have been touched, so the
    /// journal must survive (it's the only complete copy) even if the commit fails.
    commit_started: bool,
}

const BUF_CAP: usize = 64 * 1024;

impl FsWriter {
    // Waits for an in-flight flush, if any.
    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let State::Flushing(task) = &mut self.state {
            let (file, result) = ready!(Pin::new(task).poll(cx)).map_err(io::Error::other)?;
            self.state = State::Idle(Some(file));
            result?;
        }
        match self.state {
            State::Idle(_) => Poll::Ready(Ok(())),
            _ => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "writer is closed",
            ))),
        }
    }

    fn start_flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let State::Idle(file) = &mut self.state else {
            return;
        };
        let mut file = file.take().expect("idle writer has a file");
        let buf = std::mem::take(&mut self.buf);
        self.state = State::Flushing(tokio::task::spawn_blocking(move || {
            let result = file.write_all(&buf);
            (file, result)
        }));
    }
}

impl AsyncWrite for FsWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_idle(cx))?;
        this.buf.extend_from_slice(data);
        this.crc.update(data);
        this.len += data.len() as u64;
        if this.buf.len() >= BUF_CAP {
            this.start_flush();
        }
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_idle(cx))?;
        this.start_flush();
        this.poll_idle(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                State::Done => return Poll::Ready(Ok(())),
                State::Committing(task) => {
                    let result = ready!(Pin::new(task).poll(cx)).map_err(io::Error::other)?;
                    this.state = State::Done;
                    return Poll::Ready(result.map_err(io::Error::other));
                }
                _ => {
                    ready!(this.poll_idle(cx))?;
                    if !this.buf.is_empty() {
                        this.start_flush();
                        continue;
                    }
                    let State::Idle(file) = &mut this.state else {
                        continue;
                    };
                    let file = file.take().expect("idle writer has a file");
                    let ctx = this.ctx.clone();
                    let (len, crc) = (this.len, this.crc.finish());
                    this.commit_started = true;
                    this.state = State::Committing(tokio::task::spawn_blocking(move || {
                        commit(&ctx, file, len, crc)
                    }));
                }
            }
        }
    }
}

impl Drop for FsWriter {
    fn drop(&mut self) {
        // A writer dropped before `close()` started the commit never touched the object, so
        // its journal is simply removed (D63). Once the commit has started, a failure may have
        // left the object partly rewritten: the journal stays, and the next open reports it.
        if let (false, Target::Journal { name, path }) = (self.commit_started, &self.ctx.target) {
            let _ = fsops::unlink(&self.ctx.dir, name, path);
        }
    }
}

// The commit point (D43, D63).
fn commit(ctx: &WriteCtx, file: File, len: u64, crc: u32) -> Result<(), PosixError> {
    match &ctx.target {
        Target::Direct => fsops::sync(&file, &ctx.path),
        Target::Journal { name, path } => {
            // 1. Complete and persist the journal.
            journal::complete(&file, len, crc).map_err(|e| PosixError::io(path, e))?;
            fsops::sync(&file, path)?;
            fsops::sync(ctx.dir.as_fd(), &ctx.path)?;
            // 2. Rewrite the object in place (created now if it didn't exist, D67).
            let mut object =
                open_or_create(&ctx.dir, &ctx.name, ctx.dev, ctx.file_mode, &ctx.path)?;
            object
                .set_len(0)
                .map_err(|e| PosixError::io(&ctx.path, e))?;
            journal::copy_content(&file, &mut object).map_err(|e| PosixError::io(&ctx.path, e))?;
            fsops::sync(&object, &ctx.path)?;
            // 3. Drop the journal.
            fsops::unlink(&ctx.dir, name, path)?;
            fsops::sync(ctx.dir.as_fd(), &ctx.path)
        }
    }
}
