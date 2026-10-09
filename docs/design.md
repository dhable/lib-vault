# lib-vault — Design

Status: **API sketch final (v9)**; implementation not started. The sketch in `src/lib.rs` is
the source of truth. Decision log and open questions: [`decisions.md`](decisions.md).

## Purpose

`lib-vault` gives an AI harness storage for memory context, scratchpads, artifacts and similar
data. Storage is **pluggable**: the first backend is a local directory, but a database, object
store or in-memory store can sit behind the same API (D31). Goals:

- **Confinement**: a claim can only reach keys beneath the key it claimed. `Key` can't express
  `..` or an absolute path, and scoping is done with a join that can only nest (D34–D36). A
  backend must not let a key reach anything outside its own root.
- **Claims**: every access goes through a `Claim`, an exclusive, non-cloneable hold on a key and
  everything beneath it. Claims come only from the vault.
- **Exclusion across applications**: overlapping claims are refused, across processes and, for
  the filesystem backend, across unrelated applications running as different uids/gids (D5).
- **Runtime-agnostic async**: the API is `async`, object handles use the `futures` I/O traits,
  and there's no dependency on a particular runtime (D33).
- **Defense in depth**: backends use OS and kernel mechanisms wherever they help (§8, §9).

---

## 1. Threat model

| Actor | Capability | Defended? |
|---|---|---|
| AI model via harness (possibly prompt-injected) | Chooses keys, content, claim patterns | **Yes, primary adversary** |
| Cooperating app (any uid) using lib-vault | Follows the claim protocol | Yes: claim exclusion |
| Process not using lib-vault with write access to the backing storage | Can create links, rename, ignore locks | Partially (fs backend): we never follow its links or escape the root; we can't stop its writes |
| Participant acting maliciously | Can hold claims forever | **No**: lock DoS is inherent (documented) |
| Other users without access, or root | — | Only via permissions / OS configuration |

**Compile-time scoping is not a security boundary** (D20). Types and lifetimes protect against
harness *programming bugs*. The adversary is the model, which controls keys and content, not
Rust code. Security enforcement lives in key validation (§3) and in each backend (§8).

## 2. Architecture

| Component | Role |
|---|---|
| `Vault` | Cheap-to-clone handle (`Clone`, `Send + Sync`) holding `Arc<dyn Store>`, `Arc<dyn Claimer>` and the shared shutdown state `Arc<Mutex<VaultState>>`. Built with `Vault::new(store, claimer)` (D32). |
| `Claimer` | Strategy for exclusion: `try_lock(key)` makes one attempt and returns a `ClaimLock` or an error; it never waits (D32, D44). Backends: `flock` for the filesystem (§8.3); later a database row lock, a lease, an in-process map. |
| `ClaimLock` | One held lock. `release()` is synchronous and must not block; each implementation handles its own async work. `Ok` means the key can be claimed again immediately (D39). Called from `Claim::release` or `Claim::drop`. |
| `Claim` | Exclusive hold on a key and its subtree: scoped store + lock + shared vault state. `list`, `delete`, `open_read`, `open_write` on keys within it, all `&mut self`. Given up with `release()` (to see errors) or by dropping it. `Send`, never `Clone` (D37). |
| `Store` | Storage strategy: `list`, `delete`, `open_read`, `open_write` on keys. `Send + Sync + 'static`. |
| `ScopedStore` | A `Store` wrapping another `Store` and prefixing every key with a base key. This is how a `Claim` is confined (D36). |
| `ObjectReader<'a>` / `ObjectWriter<'a>` | `Pin<Box<dyn AsyncRead/AsyncWrite + Send + 'a>>`: object handles, borrowed from the claim (D38, D42). |
| `Key` | Canonical, relative, `/`-separated name. `Arc<str>` newtype (§3). |

**Claim flow:**
1. The caller awaits `vault.claim(key)`.
2. Under the vault mutex: if shutdown has started, the call fails. Otherwise the vault counts
   the claim as outstanding and builds a lock-less `Claim::new(store, key, state)`, which wraps
   the store in `ScopedStore::new(key, store)`.
3. With the mutex released, the vault awaits `claimer.try_lock(&key)`. The claimer makes one
   strategy-specific locking attempt and returns a `ClaimLock`, or an error if the key conflicts
   with a held claim. It never sees the store.
4. On success the vault stores the lock in the claim (`set_lock`) and returns it. On an error,
   or if the `claim()` future is dropped, the lock-less claim is dropped and gives its count back.
5. The claim's operations take an `Option<&Key>` **relative to the claimed key**: `None` is the
   claimed key itself, `Some(k)` a key beneath it. They can't leave the claim's subtree (D48).
6. `claim.release()` releases the lock and returns any error; dropping the claim releases it if
   that hasn't happened (errors are logged) and decrements the outstanding count, so `shutdown`
   can finish.

Because the count goes up before awaiting the claimer, shutdown also waits for claims in flight
(§4.5).

## 3. Keys

`Key` replaces `Path` in the API: storage isn't necessarily a filesystem, and `Path` allows
things a vault must not (absolute paths, `..`, OS-specific syntax).

- **Syntax** (D34): unix-like, components separated by `/`. Strictly canonical, so each key has
  exactly one spelling:
  - not empty: there is no root key (D48);
  - no leading or trailing `/`, no empty components (`a//b`);
  - no `.` or `..` components;
  - no NUL bytes.
  Anything else is rejected with a `KeyError` (`Empty`, `LeadingSlash`, `TrailingSlash`,
  `EmptyComponent`, `DotComponent`, `Nul`). Nothing is normalized.
- **Representation**: `Key(Arc<str>)`. Cloning is a refcount bump, so a `Store` can clone a
  `&Key` into a `'static` future rather than borrowing it.
- **`join(&self, &Key) -> Key`** (D35): infallible, because both sides are already canonical.
  Unlike `Path::join`, the argument never replaces the base: `a` joined with `b/c` is always
  `a/b/c`, beneath `self`.
- **Component-wise helpers**: `components`, `starts_with` (`a/bc` does *not* start with `a/b`;
  a key starts with itself), `strip_prefix` (`a/b/c` minus `a` is `b/c`; a key minus itself is
  `None`), `parent` (`None` for a single-component key), `object_name` (the last component;
  always `Some`). Claimers use `starts_with` for overlap checks; stores use `strip_prefix` to
  return keys relative to the listed key.
- **Constructors and conversions**: `Key::new`, `FromStr`, `TryFrom<&str>`, `TryFrom<String>`,
  `AsRef<str>`, `Display`. There is no `Key::root`.
- **Naming the claim itself** (D48): since there's no empty key, the whole vault can't be
  claimed, and a claim refers to its own key with `None` (§4.1).
- The portable name policy (§10) is enforced by the filesystem backend today. Moving it into
  `Key` for every backend is open (P38).

## 4. Claims

### 4.1 Scoping (D36)
A claim on `k` can touch `k` and its descendants and nothing else. `Claim::new` wraps the
vault's store in `ScopedStore::new(k, store)`: every key the claim receives is joined beneath
`k`, and since keys can't contain `..` and `join` only nests, there's no way out.

Claim operations take `Option<&Key>` (D48). `Claim` holds the `ScopedStore` and resolves the
argument with `ScopedStore::resolve`: `None` → `k`, `Some(x)` → `k.join(x)`. It then calls the
inner `Store` with that full key, so backends never see an optional or empty key. So
`claim.delete(None)` deletes the claimed object itself, and `claim.list(None)` lists everything
in the claim.

### 4.2 Ownership and threads (D37)
- `Claim` is `Send`: it can be moved to another thread or into a spawned task. This is
  inferred from its fields, not declared (no `unsafe impl`, no `negative-impl`). A
  compile-time `Claim: Send` assertion should guard it.
- Every operation takes `&mut self`, so only one caller can use a claim at a time. `&mut Claim`
  is `Send`, so a task that passes a claim to helpers like `async fn work(c: &mut Claim)` stays
  spawnable. "Takes `&mut self`" is about exclusivity; not every method changes the claim.
- `Claim` is `!Sync` as a side effect (`dyn ClaimLock` isn't `Sync`). Nothing relies on it: a
  `&Claim` can't do anything.
- `Claim` never implements `Clone`.

### 4.3 Object handles (D38, D42)
```rust
pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

pub async fn open_read<'a>(&'a mut self, key: Option<&Key>) -> Result<ObjectReader<'a>, Error>;
pub async fn open_write<'a>(&'a mut self, key: Option<&Key>) -> Result<ObjectWriter<'a>, Error>;
```
- **Pinned and boxed** so the `futures` extension methods (`read_to_end`, `write_all`, …) work
  whether or not a backend's reader is `Unpin`; **`Send`** so tasks holding one stay spawnable.
- **Borrowed from the claim.** A handle borrows `&mut Claim`, so the claim can't be released or
  dropped while a handle is alive (`drop(c)` with a live reader is a compile error, E0505).
  Drop the handle first, or scope it in a block. This ends the "stale writer after release"
  hazard for safe code; it's still harness-bug protection, not a security boundary (D20).
- **One handle at a time.** For the same reason, a claim can have only one open reader or
  writer, and `list`/`delete` can't be called while one is open. Accepted (D42).
- **Commit and flush behavior** is the `Store` implementation's to document (§5).

### 4.4 Release and `Drop` (D11, D29, D39)
```rust
pub trait ClaimLock: Send {
    /// Non-blocking. `Ok` only once the key can be claimed again immediately.
    fn release(self: Box<Self>) -> Result<(), Error>;
}
impl Claim {
    pub fn release(&mut self) -> Result<(), Error> {
        if let Some(lock) = self.lock.take() { lock.release()?; }
        Ok(())
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        let _ = self.release().map_err(|e| tracing::error!(cause = %e, "ClaimLock::release failed"));
        /* count down; at zero, take the waiters and wake them after unlocking */
    }
}
```
- **Two ways to give up a claim** (D11): `claim.release()` releases the lock and returns the
  error, if any; dropping the claim releases it if `release` wasn't called and logs any error
  with `tracing`. The lock is in an `Option` and `take()`n, so it's released at most once. A
  lock-less claim (failed or cancelled `try_lock`, §4.5) skips the release.
- **Handles first.** Readers and writers borrow the claim mutably, so they must be dropped
  before `release()` or the drop (§4.3).
- **Release is synchronous and each implementation owns its async work** (D39). The library
  never runs a future in `release` or `Drop`, so it never blocks the dropping thread or nests
  executors (the v6–v8 `block_on` hazards, P44 and P46). An implementation whose release needs
  I/O does it on its own thread or runtime. `flock` (closing fds) needs nothing.
- **What `Ok` means** (D39): the lock really is released, so the same key can be claimed again
  immediately. An implementation that can't promise that must not return `Ok` early. This also
  keeps shutdown honest: once every claim is gone, every lock is released.
- **Rules for implementors not written in the trait docs:** `release` runs on whatever thread
  calls `Claim::release` or drops the claim, possibly outside any async runtime, so it must not
  block or panic. `tokio::spawn` panics when no runtime is current, and a panic in `Drop` while
  unwinding aborts the process.
- **After `release()` the claim object still exists** (`release` takes `&mut self`). It no
  longer holds a lock, but `list`/`delete`/`open_*` still work on its scoped store, so they are
  no longer exclusive. It also still counts as outstanding until dropped, so `shutdown` waits
  for the drop, not the release. Callers should drop the claim right after releasing it (P49).
- **No implicit flush/sync.** Durability is the caller's job before releasing.

### 4.5 Shutdown (D13, D41)
`async fn shutdown(self)` sets the gate so no new claims are issued, then waits until every
outstanding claim is gone. It waits for claims only, not for other `Vault` clones (D46).
Scope is this process only.

```rust
struct VaultState { shutting_down: bool, outstanding_claims: usize, waiters: Vec<Waker> }
// Arc<Mutex<VaultState>> is held by every Vault clone and every Claim.
```
- `claim` checks `shutting_down`, counts up, and builds the `Claim` (without a lock yet) under
  the mutex, then drops the guard **before** awaiting `try_lock`, so its future is `Send` and a
  waiting claim never blocks other threads on the mutex. Counting up before the await makes
  shutdown wait for claims in flight. On success `set_lock` stores the lock; on an error or a
  dropped future, the lock-less `Claim` is dropped and its `Drop` gives the count back (D47). One mutex
  over flag and count means `claim` and `shutdown` can't miss each other's update (two atomics
  would need `SeqCst`).
- `Claim::drop` counts down after releasing the lock (if still held), and when the count reaches zero takes the
  waiters and wakes them after unlocking the mutex (a woken task may be polled on this thread
  right away and need the mutex).
- `shutdown` sets the flag, then awaits a `poll_fn` that returns `Ready` at zero and otherwise
  stores its task's `Waker` (skipping duplicates with `will_wake`). Several `Vault` clones can
  await `shutdown` at once; each adds its own waker.
- A task that holds a claim and awaits `shutdown` never finishes. Document it.

Checked with tests in a scratch copy: a refused claim and a cancelled claim both give their
count back; `shutdown` finishes when the last claim is dropped, including on another thread;
the `claim()` and `shutdown()` futures are `Send`. (These were run against sketch v8; the
counting code is unchanged in v9.) These tests aren't in the repository; §13 lists them.

## 5. Store contract

```rust
pub trait Store: Send + Sync + 'static {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>>;
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>>;
    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>>;
    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>>;
}
```
Every key a `Store` receives is a full, non-empty key; `Claim` resolves its optional,
claim-relative keys before calling the store (D48).

Object semantics (D43):
- **`list`** (D40): everything beneath `key`, as keys relative to `key`; "beneath" is
  component-wise. No paging or limits in the first revision.
- **`delete`**: deleting a key with no object succeeds, like HTTP `DELETE`.
- **`open_read`**: a key with no object is an error.
- **`open_write`**: overwrites an existing object, or creates it. When data is committed or
  flushed, and what happens to in-flight writes once the claim is released, is documented by
  each `Store` implementation. (Object stores typically commit on `close()`.)
- Handles may borrow the store (`'a`); `Claim` shortens that to the claim borrow.
- Futures borrow the store but not the key; implementations clone the key.

## 6. Claimer contract

```rust
pub trait ClaimLock: Send {
    fn release(self: Box<Self>) -> Result<(), Error>;
}
pub trait Claimer: Send + Sync + 'static {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>>;
}
```
These are documented on the traits in `src/lib.rs`:
- **Exclusive only** (D2). **The claimer only locks** (D32): it returns a `ClaimLock`, and the
  vault builds the `Claim`.
- **Exclusion** (D44, resolves P36): a claim on `k` covers `k` and every key beneath it,
  compared component-wise (`Key::starts_with`). While `a/b` is held, `try_lock` refuses `a/b`,
  any descendant (`a/b/c`) and any ancestor (`a`). Disjoint keys (`a/b`, `a/c`)
  don't conflict. Exclusion holds between all holders sharing the backing storage: tasks,
  threads, other `Vault`s in the process, other processes. A claimer that only sees its own
  process says so.
- **No waiting** (D44, resolves P40): `try_lock` makes one attempt; on conflict it fails at
  once. Callers that want to wait retry with their own backoff and timeout (D23). With no
  waiting inside the library, two claims can't deadlock on each other (P19).
- **Cancellation**: if the `try_lock` future is dropped before completing, anything partially
  acquired is released; a lock is never held without a returned `ClaimLock`.
- **Errors**: conflict, or the backend refusing or being unreachable.
- **Pairing with a store** (D45, resolves P37): some claimers lock the store's own storage
  (`FlockClaimer` locks `FsStore`'s directories). Each claimer documents which stores it works
  with; `Vault::new` doesn't check.
- **`ClaimLock::release`** is synchronous and non-blocking; the implementation handles any
  async work itself. It returns `Ok` only once the key can be claimed again immediately (D39,
  §4.4). It's called from `Claim::release` or `Claim::drop`.

## 7. Public API

Final sketch (`src/lib.rs`), abbreviated:

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {                                          // D12
    ShuttingDown,
    Key(#[from] KeyError),
    AlreadyClaimed { key: Key },
    NotFound { key: Key },
    ClaimError(#[source] Box<dyn std::error::Error + Send + Sync>),
    StoreError(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(Arc<str>);                                 // §3
impl Key {
    pub fn new(s: &str) -> Result<Key, KeyError>;
    pub fn join(&self, other: &Key) -> Key;
    pub fn starts_with(&self, base: &Key) -> bool;
    pub fn strip_prefix(&self, base: &Key) -> Option<Key>;
    pub fn parent(&self) -> Option<Key>;
    pub fn object_name(&self) -> Option<&str>;
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str>;
}

pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

#[derive(Clone)]
pub struct Vault { store: Arc<dyn Store>, claimer: Arc<dyn Claimer>, state: Arc<Mutex<VaultState>> }
impl Vault {
    pub fn new(store: impl Store, claimer: impl Claimer) -> Self;
    pub async fn claim(&self, key: Key) -> Result<Claim, Error>;
    pub async fn shutdown(self);
}

pub struct Claim { store: ScopedStore, lock: Option<Box<dyn ClaimLock>>, state: Arc<Mutex<VaultState>> }
impl Claim {                                              // Send, never Clone
    // `None` = the claimed key itself (D48)
    pub async fn list(&mut self, prefix: Option<&Key>) -> Result<Vec<Key>, Error>;
    pub async fn delete(&mut self, key: Option<&Key>) -> Result<(), Error>;
    pub async fn open_read<'a>(&'a mut self, key: Option<&Key>) -> Result<ObjectReader<'a>, Error>;
    pub async fn open_write<'a>(&'a mut self, key: Option<&Key>) -> Result<ObjectWriter<'a>, Error>;
    pub fn release(&mut self) -> Result<(), Error>;
}
impl Drop for Claim { /* self.release() (errors logged), then count down */ }

pub trait ClaimLock { /* §6 */ }
pub trait Claimer { /* §6 */ }
pub trait Store { /* §5 */ }
pub struct ScopedStore { base: Key, inner: Arc<dyn Store> }   // ScopedStore::new(base, inner)
```

**Async style** (D33): inherent methods are `async fn`; trait methods return `BoxFuture`. A
trait using `async fn` can't be used as `dyn Trait` and can't promise its futures are `Send`.
An `async fn`'s `Send`-ness is inferred, so it can be lost silently (as the v6 `claim`, which
held a `MutexGuard` across `.await`, showed); §13 asserts it.

**Build status:** builds; the 10 unit tests pass, including a compile-time `Claim: Send`
check; `cargo doc` is clean; the `Claim::list` example is `ignore`d. Known leftovers:
- a stale `REVIEW(nit)` comment above the `try_lock` docs (what it flags is fixed);
- the test playground's `MemStore`/`MemClaimer` give unused-variable and dead-code warnings;
- doc comments have some typos.

**History.** v9 made `ClaimLock::release` synchronous (it was a `BoxFuture` run with `block_on`
in `Drop` in v6–v8) and turned `Claim::release` into a sync `&mut self` method (an
`async fn release(self)` in v5–v8). Removed since v5: `ClaimLock::try_release`, the
`shutting_down: Arc<AtomicBool>` gate, `Claimer::try_claim` (renamed `try_lock`). Since v4:
`StoreObject`, `Claim::open`, the claimer building the `Claim`, the `negative-impl` dependency.
Since v3: `Vault::new(root)` / `new_shared`, `shutdown_timeout`, `Claim<'a>` lifetimes,
`sub_claim`, `open() -> &File`, `open_dir() -> &Dir`, `Dir`.

## 8. Filesystem backend (`FsStore` + `FlockClaimer`)

The first backend. This was the whole design before v4; it now describes one `Store` and one
`Claimer` that must be used together (D45).

### 8.1 Confinement: race-free, handle-relative resolution

Checking path strings (`canonicalize` + `starts_with`, or `symlink_metadata` then `open`) leaves a
TOCTOU window, because an external process can swap a component for a symlink between check and use.
Instead:

1. The store opens the root directory once and keeps the handle. Every later operation is relative to a held handle, so renaming or replacing the root path afterwards has no effect.
2. Keys are already free of `..`/`.`/empty/NUL/absolute components (§3). The backend adds the name policy (§10).
3. Resolution:
   - **Linux ≥ 5.6**: `openat2(dirfd, p, RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)`. On older kernels, fall back to per-component `openat(…, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)`.
   - **macOS**: per-component `openat(…, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)`. Leaf opens add `O_NOFOLLOW_ANY` (macOS 11+, P25). There is no `RESOLVE_NO_XDEV` equivalent, so mount crossings are detected by comparing `st_dev` after each step.
   - **Later**: FreeBSD 13+ `O_RESOLVE_BENEATH`; Windows `NtCreateFile` with `RootDirectory` + `FILE_FLAG_OPEN_REPARSE_POINT`.
4. After every open, `fstat` the **handle** (not the path) and enforce §8.2.

The lock protocol (§8.3) needs a handle per path component anyway, so the per-component walk is
the primary path. std doesn't expose `openat`/`openat2`/`O_NOFOLLOW_ANY`, so internals use a thin
syscall crate (`rustix`, P12).

These are blocking syscalls behind an async API. How they run without depending on a runtime
(an internal thread pool, or a caller-provided spawner) is open (P39).

### 8.2 Links and special files (D17)

| Case | Policy | Mechanism |
|---|---|---|
| Symlink anywhere in a path | Refuse | resolve flags / `O_NOFOLLOW`; no symlink-creation API |
| Regular file with `st_nlink > 1` | Refuse | `fstat` on the opened fd |
| Hard link created after open | Residual risk, documented | data stays on the vetted inode |
| Mount / bind mount inside the vault | Refuse | `RESOLVE_NO_XDEV` / `st_dev` equals the root's |
| FIFO / socket / device node | Refuse | open `O_NONBLOCK` → `fstat` → require REG/DIR → clear `O_NONBLOCK` (a FIFO open would otherwise block) |
| `list` entries | Classify via `fstatat(AT_SYMLINK_NOFOLLOW)`; links and specials are skipped and never traversed | |
| Recursive delete | Handle-relative walk + `unlinkat`; never follows | |
| Modes | Never set exec/setuid/setgid. Files `0600`, dirs `0700` (`0660` / `2770` in group-sharing mode, §9) | explicit `fchmod` after create, independent of umask |

Windows (later): reject reparse points (symlinks, junctions, mount points), alternate data streams
(`name:stream`) and 8.3 short names.

### 8.3 `FlockClaimer`: cross-process exclusion (D16)

**Mechanism.** Options considered for coordinating unrelated processes running as different uids:

| Mechanism | Cross-uid | Auto-release on death | Needs shared writable state | Verdict |
|---|---|---|---|---|
| **`flock` on the vault's own directory/file inodes** | ✔ (anyone who can `open` the object) | ✔ | **No** | **Chosen** |
| `flock` on lock files in a `.vault/locks/` dir | ✔ | ✔ | Yes: participants could unlink or replace lock files (split-brain) | Rejected |
| OFD byte-range locks on one shared file | ✔ | ✔ | Yes; Linux-only | Rejected |
| Classic POSIX `fcntl` locks | ✔ | ✔ | Yes; lost on *any* `close()` in the process | Rejected |
| Mandatory locking | — | — | — | Removed in Linux 5.15 |
| Leases (`F_SETLEASE`) | Owner only | — | — | Rejected |
| SysV / POSIX semaphores | Permission- and IPC-namespace-scoped | SysV only with `SEM_UNDO` | Named objects | Rejected |
| Abstract Unix socket as mutex | ✔ | ✔ | No | Exclusive-only, no hierarchy, Linux-only. Rejected |

The kernel's `flock` table is global and keyed by inode. Uid doesn't matter, only the ability to
open the object. std exposes it: `File::open` works on Unix directories, and
`File::lock`/`lock_shared`/`try_lock`/`try_lock_shared`/`unlock` (Rust 1.89+) map to `flock`.
Locks belong to the open file description, so separate opens within one process also exclude each other.

**Protocol "inode-lock v1"** (frozen; every lib-vault version must speak it). To claim
`T = r/c1/…/cn` (`r` = vault root):

1. Walk from the root handle, opening each component no-follow.
2. In strict top-down order: `lock_shared` on `r` and every ancestor, then `lock` (exclusive) on `T`.
3. **Verify after lock** at each level: `fstatat(parent_fd, name, AT_SYMLINK_NOFOLLOW)` must match the locked fd's `(st_dev, st_ino)`, and `st_nlink > 0`. On a mismatch (renamed or removed while waiting), unlock that level and retry, with a bounded retry count (P23).
4. The claim holds n+1 fds (ancestors shared + target exclusive) for its lifetime. Closing them releases the locks.

Correctness:
- `a/b/c` vs `a/b/d`: both share only ancestors, so they are compatible.
- `a/b` vs held `a/b/c`: the exclusive lock on `a/b` is blocked by the holder's shared lock ✔. `a/b/c/d` vs held `a/b/c`: the shared lock on `a/b/c` is blocked by the holder's exclusive lock ✔.
- Fixed top-down order prevents deadlock between claimants.
- **Aliasing is solved for free**: `Notes` and `notes` on a case-insensitive filesystem are the same inode, so they get the same lock. Hard links are refused. A rename by an outsider moves the lock with the inode.

Rules and consequences:
- **The target must exist** before it can be locked, so claiming a missing key must create it. As a file or a directory is open (P1, P8).
- **Deleting a claimed target** happens via the held parent fd. Waiters then wake on a dead inode, fail verify and retry.
- **Read permission is required**: opening a directory for `flock` needs `r`, so every participant needs `r-x` along its claim paths. Otherwise the claim fails with `PermissionDenied`.
- **Nested vault roots** (app A at `/data/v`, app B at `/data/v/sub`): also take **best-effort shared locks on every ancestor above the vault root up to `/`** (D28). Unopenable ancestors are skipped, with a `tracing` warning (P9). `/` and system directories are refused as roots.
- **In-process registry**: keyed by `(dev, ino)`, it gives fast conflict errors for claims held in this process. `flock` stays the source of truth.
- **Acquisition** (D18, D44): each `try_lock` is one non-blocking pass: `try_lock_shared`/`try_lock` at every level, and on `WouldBlock` release everything taken and fail with a conflict. It never parks a thread in blocking `flock(2)`. Callers that want to wait retry with backoff (about 1 ms → 50 ms with jitter) and their own timeout.
- **Fairness**: `flock` has no queueing, so waiters can starve.
- **Filesystems**: local only, since NFS `flock` emulation and directory locking are unreliable. Detect with `statfs`: `f_type` magic numbers on Linux, `MNT_LOCAL` on macOS (P16).
- **File claims** (D19): no atomic rename-replace of a claimed file. Temp-file + rename creates a new inode: the holder keeps locking the orphaned old one while a waiter locks the new one, and both believe they hold the claim. Write in place. Open `O_RDWR`, falling back to `O_RDONLY` when a cross-uid participant has only read permission (P24).
- **Windows (later)**: `LockFileEx` can't lock directories. Candidate: directory handles opened with share modes (`FILE_SHARE_READ` = shared, no sharing = exclusive).

## 9. OS-level security (filesystem backend)

The library runs in-process inside the harness, so process-wide self-sandboxing (Landlock,
seccomp) is out of scope (D4).

Checks when an `FsStore` is created:
- The root is a real directory (opened no-follow), on a local filesystem, and not `/` or a system directory.
- Creation is idempotent across apps: `mkdir` tolerating `EEXIST`, then validate. No shared metadata is written.
- **Sharing mode** (formerly D30, now an `FsStore` constructor concern):
  - private: root owned by the euid, mode `0700`, files `0600`;
  - group: root group-owned by a given gid, setgid `2770`, files `0660`. Different-uid apps need this mode or admin-configured ACLs (P10).
  - A root that doesn't match the requested mode is rejected. Nothing world-writable is ever accepted.
- Linux: emit a `tracing` warning when `fs.protected_symlinks` / `fs.protected_hardlinks` are disabled.

Deployment guide (documentation):
- Run harness instances as dedicated uids that share a vault group.
- Linux: mount the vault filesystem with `nosymfollow,nodev,nosuid,noexec`.
- macOS: a dedicated APFS volume (size-limited with a quota) mounted `nodev,nosuid,noexec`.
- Cap usage with a size-capped filesystem, project quotas or `RLIMIT_FSIZE` (D8).
- Provide AppArmor / SELinux profile examples.
- Tool subprocesses spawned by the harness can be Landlocked to the vault by the harness.

## 10. AI-harness concerns

- **Name policy (strict portable ASCII, D6)**, enforced by the fs backend on top of `Key` (P38):
  - components match `[A-Za-z0-9._ -]+`;
  - no leading `-` or space, no trailing `.` or space;
  - no Windows reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`, with any extension);
  - component ≤ 255 bytes; key depth and length are capped;
  - the `.vault` prefix is reserved;
  - **case-collision check** on create: creating `Foo` is refused when `foo` exists (P20).
  This rules out terminal-escape, bidi and homoglyph spoofing when names are shown to humans.
- **Claim DoS by the model**: no acquisition timeouts in the library (D23). The harness wraps
  `claim()` in a timeout and owns hold-time and claim-count policy.
- **Memory poisoning / persistent prompt injection**: the library can't judge content. Future work: optional provenance and tamper detection (P11).
- **Dangerous-by-convention names** (`.git`, `.envrc`, `.vscode`, `Makefile`, `*.desktop`…): optional configurable denylist (P11). Never create executables.
- **Crash consistency**: in-place writes can leave partial files (P21).
- **fd hygiene** (fs): `O_CLOEXEC` everywhere. A child process inheriting a lock fd would keep a claim alive.
- **fd budget** (fs): each claim holds depth+1 fds, plus ancestors above the root. Cap key depth.

## 11. Async notes

Async is the base API (D33), not a later addition. Notes carried over from the old v2 plan:
- Traits return `futures::future::BoxFuture` (boxed, `Send`); inherent methods are `async fn`
  (§7). Handles use the `futures` I/O traits; tokio users adapt with `tokio_util::compat`.
- **`Send` futures**: claim methods take `&mut self`, so using a claim keeps futures `Send`
  (§4.2), readers/writers are `Send` (§4.3), and so are the `claim()` and `shutdown()` futures.
- **No async in `release` or `Drop`**: `ClaimLock::release` is synchronous and must not block;
  a lock that needs async I/O to release runs it on its own thread or runtime (D39, §4.4).
- **Flush before unlock**: a backend whose writes complete in the background (e.g. a thread
  pool doing file I/O) must make sure in-flight writes land before the claim's lock is
  released, including on `Drop`, or exclusion is broken. Handles borrow the claim (§4.3), so
  they're dropped before the claim is released; a backend whose writer `Drop` leaves writes in
  flight must still finish them before its `ClaimLock::release` returns `Ok`.
- **Cancellation**: dropping a `claim()` future must release any partially acquired locks.
- **Blocking work**: backends with blocking syscalls need somewhere to run them (P39).

## 12. Module layout (when coding starts)

| Module | Contents |
|---|---|
| `lib.rs` | re-exports; `tracing` spans/events for diagnostics (D26) |
| `error.rs` | `Error` enum (D12) |
| `key.rs` | `Key`, `KeyError` |
| `vault.rs` | `Vault`, `VaultState` (shutdown flag, outstanding count, waiters) |
| `claim.rs` | `Claim`, release / `Drop` |
| `store.rs` | `Store`, `ScopedStore`, `ObjectReader` / `ObjectWriter` |
| `claimer.rs` | `Claimer`, `ClaimLock` |
| `backend/fs/` | `FsStore`, `FlockClaimer`, name policy, inode-lock v1, registry, `sys/{linux,macos}.rs` |
| `backend/memory.rs` *(likely)* | in-memory `Store` + `Claimer` + `ClaimLock`, also useful as the test double |
| `benches/` *(future)* | fs performance harness (D28, P26) |

## 13. Verification strategy

- **`Key`** (done, unit tests in `src/lib.rs`): canonical keys accepted, each rejection case,
  the empty key rejected, join associativity, join always nests, component-wise
  `starts_with`/`strip_prefix`, `parent`/`object_name`, parse/display round trip.
- **Compile-time checks**: `Claim: Send` (done), `Vault: Send + Sync + Clone`, and that the futures
  of `Vault::claim`, `Vault::shutdown` and a typical claim-using `async` block that holds a
  reader/writer across `.await` are `Send`. Tests that `drop(claim)` or `claim.release()` with a
  live reader don't compile (`compile_fail` doctests).
- **Core, against an in-memory backend**: scoped claims can't reach outside their key (D36);
  overlapping claims conflict in both directions and disjoint ones don't (D44);
  `ClaimLock::release` runs exactly once whether the claim is released, dropped, or both, and
  `release()` returns its error; a released key can be claimed again immediately (D39); shutdown
  refuses new claims, waits for outstanding and in-flight claims, finishes when the last one is
  dropped (including inside an executor and on another thread), and isn't blocked by a refused
  or cancelled claim; several tasks awaiting `shutdown` all finish.
- **Doc examples** compile (`cargo test --doc`), or are marked `ignore`.
- **Filesystem backend**:
  - race test: a thread flips a path component between a directory and a symlink while another opens in a loop; nothing may escape the root;
  - pre-planted symlink, hard link, FIFO and bind mount are refused;
  - multi-process tests (re-exec the test binary): overlapping vs disjoint claims, `kill -9` of the holder releases its locks, a waiter on a removed directory hits verify and retries;
  - cross-uid: a CI job in a root-capable container runs two children under different uids sharing a group-mode vault;
  - nested vault roots: an outer-root exclusive claim blocks an inner-root claim;
  - CI matrix: Linux (with `openat2` plus a fallback-path run) and macOS.
- *(future)* Performance harness: per-claim fd and syscall counts, acquisition latency vs key depth and contention (D28).
