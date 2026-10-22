# lib-vault — Design

Status: the core API (`lib-vault`, sketch v10) and four backends are implemented:
`lib-vault-memory` (D51), `lib-vault-posix` (D73), `lib-vault-sqlite` (D78, D79, typed
values) and `lib-vault-clickhouse` (D82, D83, typed values, Keeper-backed claims). The code
is the source of truth. Decision log and open questions: [`decisions.md`](decisions.md).

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
- **Runtime-agnostic async**: the API is `async` and the core crate has no runtime dependency,
  so a backend can target any runtime (D33). Two I/O drivers cover work-stealing runtimes
  (`AsyncIoDriver`, `futures` I/O traits, `Send`) and thread-per-core runtimes (`OwnedBufDriver`, owned
  `Bytes` chunks, `!Send`) (D58, D74). Each backend documents its runtime: the memory backend
  needs none, the filesystem backend requires tokio (D56).
- **Supported platforms** (filesystem backend): Linux ≥ 5.6 and macOS ≥ 15, Rust ≥ 1.89
  (D53–D55).
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
| `Vault<D>` | Cheap-to-clone handle holding the store as `D::StorePtr` (D74), `Arc<dyn Claimer>` and the shared shutdown state `Arc<Mutex<VaultState>>`. Built with `Vault::new(store, claimer)` (D32). `Send + Sync` in `AsyncIoDriver`, `!Send` in `OwnedBufDriver`. |
| `IoDriver` (`io_driver`) | Open marker trait choosing the I/O types: `Reader`, `Writer`, future type `Fut`, and `StorePtr`. `lib-vault` provides the byte drivers `AsyncIoDriver` and `OwnedBufDriver`; backends may define their own, e.g. typed values for a structured store (D58, D74, D77). |
| `Claimer` | Strategy for exclusion: `try_lock(key)` makes one attempt and returns a `ClaimLock` or an error; it never waits (D32, D44). Backends: `flock` for the filesystem (§8.3); later a database row lock, a lease, an in-process map. |
| `ClaimLock` | One held lock. `release()` is synchronous and must not block; each implementation handles its own async work. `Ok` means the key can be claimed again immediately (D39). Called from `Claim::release` or `Claim::drop`. |
| `Claim<D>` | Exclusive hold on a key and its subtree: the claimed key, the store pointer, the lock and the shared vault state. `list`, `delete`, `open_read`, `open_write` on keys within it, all `&mut self`; keys are resolved against the claimed key (D36). Given up with `release()` (to see errors) or by dropping it. Never `Clone`; `Send` in `AsyncIoDriver` only (D37). |
| `Store<D>` | Storage strategy: `list`, `delete`, `open_read`, `open_write` on full keys, returning `D::Fut` futures. Only `'static`; `Send`/`Sync` come from the driver's store pointer (D74). |
| `D::Reader<'a>` / `D::Writer<'a>` | Object handles, borrowed from the claim (D38, D42): `AsyncRead`/`AsyncWrite` boxes in `AsyncIoDriver`, a `Bytes` stream and an `ObjectWrite` box in `OwnedBufDriver` (§4.3). |
| `Key` | Canonical, relative, `/`-separated name. `Arc<str>` newtype (§3). |

**Claim flow:**
1. The caller awaits `vault.claim(key)`.
2. Under the vault mutex: if shutdown has started, the call fails. Otherwise the vault counts
   the claim as outstanding and builds a lock-less `Claim::new(store, key, state)`, keeping the
   claimed key as the claim's base.
3. With the mutex released, the vault awaits `claimer.try_lock(&key)`. The claimer makes one
   strategy-specific locking attempt and returns a `ClaimLock`, or an error if the key conflicts
   with a held claim. It never sees the store.
4. On success the vault stores the lock in the claim (`set_lock`) and returns it. On an error,
   or if the `claim()` future is dropped, the lock-less claim is dropped and gives its count back.
5. The claim's operations take a key **relative to the claimed key** as `impl AsKeyRef`: a
   `&str`, a `&Key`, or `None` for the claimed key itself. They can't leave the claim's
   subtree (D48).
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
- **Name policy** (D6, enforced by `Key::validate` for every backend): each component
  - uses only `[A-Za-z0-9._ -]`;
  - doesn't start with `-` or a space, or end with `.` or a space;
  - isn't a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`,
    with or without an extension, in any case) and doesn't start with `.vault` (reserved, any
    case);
  - is at most `MAX_COMPONENT_LEN` = 255 bytes.

  A whole key has at most `MAX_DEPTH` = 32 components and `MAX_KEY_LEN` = 1024 bytes.
- **Errors**: anything else is rejected with a `KeyError`: `Empty`, `LeadingSlash`,
  `TrailingSlash`, `EmptyComponent`, `DotComponent`, `Nul`, `InvalidChar`, `BadComponentStart`,
  `BadComponentEnd`, `Reserved`, `ComponentTooLong`, `TooDeep`, `TooLong`. Nothing is
  normalized.
- **Representation**: `Key(Arc<str>)`. Cloning is a refcount bump, so a `Store` can clone a
  `&Key` into a `'static` future rather than borrowing it.
- **`join(&self, &Key) -> Result<Key, KeyError>`** (D35): unlike `Path::join`, the argument
  never replaces the base: `a` joined with `b/c` is always `a/b/c`, beneath `self`. Both sides
  are already valid, so the only possible errors are `TooDeep` and `TooLong`, when the joined
  key exceeds the caps. That keeps every `Key`, including a claim's resolved full keys, within
  the policy.
- **Component-wise helpers**: `components`, `starts_with` (`a/bc` does *not* start with `a/b`;
  a key starts with itself), `strip_prefix` (`a/b/c` minus `a` is `b/c`; a key minus itself is
  `None`), `parent` (`None` for a single-component key), `object_name` (the last component;
  always `Some`). Claimers use `starts_with` for overlap checks; stores use `strip_prefix` to
  return keys relative to the listed key.
- **Constructors and conversions**: `Key::new`, `FromStr`, `TryFrom<&str>`, `TryFrom<String>`,
  `From<&Key>` (a cheap clone), `AsRef<str>`, `Display`. There is no `Key::root`.
- **Naming the claim itself** (D48): since there's no empty key, the whole vault can't be
  claimed, and a claim refers to its own key with `None` (§4.1).
- **Case collisions** can't be checked by `Key` (they depend on what's stored), so they're a
  `Store` contract rule for every backend: creating `Foo` fails when `foo` exists (§5, D6).

## 4. Claims

### 4.1 Scoping (D36)
A claim on `k` can touch `k` and its descendants and nothing else. The claim keeps `k` and
joins every key it receives beneath it, and since keys can't contain `..` and `join` only nests,
there's no way out.

Claim operations take `key: impl AsKeyRef` (D48). The `AsKeyRef` trait is implemented for
`&str`, `&Key` and `Option<&Key>`, so callers can write `claim.open_read("notes.md")`,
`claim.open_read(&key)` or `claim.open_read(None)`. A `&str` is validated with `Key::new` when
the method is called, and an invalid one returns `Error::Key`. There's no impl for an owned
`Key` (pass `&key`), and only one `Option` impl, so a bare `None` is unambiguous.

`Claim` resolves the argument with a private `resolve_key(k, key)`: `None` →
`k`, `Some(x)` → `k.join(x)`, which can fail with `Error::Key` if the full key would exceed the
depth or length cap. It then calls the inner `Store` with that full key, so backends
never see an optional or empty key. So `claim.delete(None)` deletes the claimed object itself,
and `claim.list(None)` lists everything in the claim.

### 4.2 Ownership and threads (D37)
- **`AsyncIoDriver`:** `Claim<AsyncIoDriver>` is `Send`: it can be moved to another thread or into a
  spawned task. This is inferred from its fields, not declared (no `unsafe impl`, no
  `negative-impl`); a compile-time test guards it, along with `Vault<AsyncIoDriver>: Send + Sync +
  Clone` and `Send` `claim()`/`shutdown()` futures.
- **`OwnedBufDriver`:** `Claim<OwnedBufDriver>` and `Vault<OwnedBufDriver>` hold the store in an `Rc` and are
  deliberately `!Send` (D74): one vault per thread, as thread-per-core runtimes expect.
- Every operation takes `&mut self`, so only one caller can use a claim at a time. In
  `AsyncIoDriver`, `&mut Claim` is `Send`, so a task that passes a claim to helpers like
  `async fn work(c: &mut Claim<AsyncIoDriver>)` stays spawnable. "Takes `&mut self`" is about
  exclusivity; not every method changes the claim.
- `Claim` is `!Sync` as a side effect (`dyn ClaimLock` isn't `Sync`). Nothing relies on it: a
  `&Claim` can't do anything.
- `Claim` never implements `Clone`.

### 4.3 Object handles and I/O drivers (D38, D42, D58)
```rust
// lib_vault::io_driver::async_io — work-stealing runtimes (tokio, async-std, smol)
pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

// lib_vault::io_driver::owned_buf — thread-per-core, completion-based runtimes (monoio, tokio-uring)
pub type ObjectReader<'a> = LocalBoxStream<'a, Result<Bytes, Error>>;
pub trait ObjectWrite {
    fn write(&mut self, chunk: Bytes) -> LocalBoxFuture<'_, Result<(), Error>>;
    fn close(self: Box<Self>) -> LocalBoxFuture<'static, Result<(), Error>>;
}
pub type ObjectWriter<'a> = Box<dyn ObjectWrite + 'a>;

impl<D: IoDriver> Claim<D> {
    // `impl Future` rather than `async fn`, so tasks awaiting them can be spawned (D80).
    pub fn open_read<'a>(&'a mut self, key: impl AsKeyRef) -> impl Future<Output = Result<D::Reader<'a>, Error>> + 'a;
    pub fn open_write<'a>(&'a mut self, key: impl AsKeyRef) -> impl Future<Output = Result<D::Writer<'a>, Error>> + 'a;
}
```
- **`AsyncIoDriver` handles** borrow the caller's buffer through the `futures` I/O traits. They're
  pinned and boxed so the extension methods (`read_to_end`, `write_all`, …) work whether or not
  a backend's reader is `Unpin`, and `Send` so tasks holding one stay spawnable.
- **`OwnedBufDriver` handles** pass ownership of `Bytes` chunks, which is what completion-based I/O
  needs (the kernel owns the buffer until the operation completes). Nothing is `Send`. The writer
  is a plain `Box` so `close(self: Box<Self>)` can be called; `close()` is the commit point.
- **The driver is a type parameter**, not a Cargo feature, so both can be used in one build
  (D58).
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
    pub fn release(mut self) -> Result<(), Error> { release_lock(&mut self) }
}   // `self` drops here, so Drop runs and counts down at once
fn release_lock(claim: &mut Claim) -> Result<(), Error> {
    if let Some(lock) = claim.lock.take() { lock.release()?; }
    Ok(())
}
impl Drop for Claim {
    fn drop(&mut self) {
        let _ = release_lock(self).map_err(|e| tracing::error!(cause = %e, "ClaimLock::release failed"));
        /* count down; at zero, take the waiters and wake them after unlocking */
    }
}
```
- **Two ways to give up a claim** (D11): `claim.release()` consumes the claim, releases the
  lock and returns the error, if any; dropping the claim releases it and logs any error with
  `tracing`. Both go through the private `release_lock`, and the lock is an `Option` that is
  `take()`n, so it's released at most once. A lock-less claim (failed or cancelled `try_lock`,
  §4.5) skips the release.
- **No claim outlives its lock** (resolves P49). Because `release` takes `self`, a released claim
  can't be used for `list`/`delete`/`open_*` any more, and since `self` is dropped at the end of
  `release`, the outstanding count goes down immediately, so `shutdown` doesn't wait for a
  separate drop.
- **Errors can't be retried.** If `ClaimLock::release` fails, the claim is gone anyway; the
  caller sees the error but can't try again. (The lock object is consumed by its own
  `release(self: Box<Self>)` regardless.)
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

Covered by tests: `lib-vault`'s unit tests (a cancelled claim and a failing release give
their count back) and `lib-vault-memory/tests/shutdown.rs` (waiting, waking from another thread,
several callers, refused claims).

## 5. Store contract

```rust
pub trait Store<D: IoDriver>: 'static {
    fn list(&self, key: &Key) -> D::Fut<'_, Result<Vec<Key>, Error>>;
    fn delete<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<(), Error>>;
    fn open_read<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<D::Reader<'a>, Error>>;
    fn open_write<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<D::Writer<'a>, Error>>;
}
```
**Thread-safety comes from the driver, not the trait** (D74): `Store` has no `Send`/`Sync`
supertraits, so an `OwnedBufDriver` store can hold `!Send` runtime state. The driver's `StorePtr` adds
them where needed: a `Vault<AsyncIoDriver>` holds `Arc<dyn Store<AsyncIoDriver> + Send + Sync>`, a
`Vault<OwnedBufDriver>` holds `Rc<dyn Store<OwnedBufDriver>>`. `Vault::new<S>(store: S, claimer)` requires `D: IntoStorePtr<S>`, which each driver implements
for every store that meets its bounds. The driver is the implementing type
(`impl<S> IntoStorePtr<S> for MyDriver`) so drivers outside `lib-vault` can implement it; the
reverse, `impl<S> IntoStorePtr<MyDriver> for S`, breaks the orphan rule (D77).

Every key a `Store` receives is a full key that already satisfies the name policy and caps
(§3); `Claim` resolves its optional, claim-relative keys before calling the store (D48).

**Case collisions** (D6): `open_write` must fail if the key differs only by ASCII case from an
existing object's key, so `Foo` and `foo` never coexist. This is the one name rule that needs
stored state, so every backend enforces it rather than `Key`. It's race-free within one claim;
to create objects whose names might collide, claim their parent directory. Two concurrent claims
on sibling keys that differ only by case can both pass the check (D65).

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
These are documented on the traits in `lib-vault/src/claimer.rs`:
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
- **Pairing with a store** (D45, D75, resolves P37): the store and the claimer are chosen
  separately, and a vault is always built with `Vault::new(store, claimer)`. Pairing is
  one-directional: some claimers lock the store's own storage (`FsClaimer` locks lock files in
  `FsStore`'s tree, so `FsClaimer::new` takes the `&FsStore`), but stores never depend on a
  claimer (`FsStore` works with `FsClaimer`, `MemoryClaimer` or a future distributed claimer).
  Each claimer documents which stores it works with; `Vault::new` doesn't check. Every process
  sharing a store's data must use the same kind of claimer, since different claimers don't
  see each other's claims; this is documented, not detected.
- **`ClaimLock::release`** is synchronous and non-blocking; the implementation handles any
  async work itself. It returns `Ok` only once the key can be claimed again immediately (D39,
  §4.4). It's called from `Claim::release` or `Claim::drop`.
- **Not driver-specific:** `Claimer: Send + Sync`, `try_lock` returns `BoxFuture`, and
  `ClaimLock: Send`, in both I/O drivers. An owned-buffer backend's claimer therefore can't do
  runtime I/O while locking; it must lock without the runtime (as `flock` does). Making the
  claimer traits driver-generic is open (P50).

## 7. Public API

Current API (`lib-vault/src/`), abbreviated:

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
    pub fn join(&self, other: &Key) -> Result<Key, KeyError>;   // TooDeep / TooLong
    pub fn starts_with(&self, base: &Key) -> bool;
    pub fn strip_prefix(&self, base: &Key) -> Option<Key>;
    pub fn parent(&self) -> Option<Key>;
    pub fn object_name(&self) -> Option<&str>;
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str>;
}
pub const MAX_COMPONENT_LEN: usize = 255;
pub const MAX_DEPTH: usize = 32;
pub const MAX_KEY_LEN: usize = 1024;

pub mod io_driver {
    pub trait IoDriver: Sized + 'static {                     // open: backends may add drivers (D77)
        type Reader<'a>;
        type Writer<'a>;
        type Fut<'a, T: 'a>: Future<Output = T> + 'a;
        type StorePtr: Clone + Deref<Target: Store<Self>>;     // D74
    }
    pub trait IntoStorePtr<S>: IoDriver { fn into_store_ptr(store: S) -> Self::StorePtr; }
    pub struct AsyncIoDriver;    // BoxFuture, AsyncRead/AsyncWrite, Arc<dyn Store<_> + Send + Sync>
    pub struct OwnedBufDriver;   // LocalBoxFuture, Bytes stream / ObjectWrite, Rc<dyn Store<_>>
    pub mod async_io  { /* ObjectReader, ObjectWriter (§4.3) */ }
    pub mod owned_buf { /* ObjectReader, ObjectWrite, ObjectWriter (§4.3) */ }
}

pub struct Vault<D: IoDriver> { store: D::StorePtr, claimer: Arc<dyn Claimer>, state: Arc<Mutex<VaultState>> }
impl<D: IoDriver> Clone for Vault<D> { /* by hand: no `D: Clone` bound */ }
impl<D: IoDriver> Vault<D> {
    pub fn new<S: Store<D>>(store: S, claimer: impl Claimer) -> Self where D: IntoStorePtr<S>;
    pub async fn claim(&self, key: Key) -> Result<Claim<D>, Error>;
    pub async fn shutdown(self);
}

pub trait AsKeyRef { fn into_key(self) -> Result<Option<Key>, KeyError>; }
// implemented for &str (validated), &Key, Option<&Key>

pub struct Claim<D: IoDriver> { base: Key, store: D::StorePtr, lock: Option<Box<dyn ClaimLock>>, state: Arc<Mutex<VaultState>> }
impl<D: IoDriver> Claim<D> {                                // never Clone; Send in AsyncIoDriver
    // key: "a/b", &key, or None (= the claimed key itself) (D48)
    // `impl Future`, not `async fn` (D80): the key is resolved when called, its error
    // delivered when awaited
    pub fn list(&mut self, prefix: impl AsKeyRef) -> impl Future<Output = Result<Vec<Key>, Error>> + '_;
    pub fn delete(&mut self, key: impl AsKeyRef) -> impl Future<Output = Result<(), Error>> + '_;
    pub fn open_read<'a>(&'a mut self, key: impl AsKeyRef) -> impl Future<Output = Result<D::Reader<'a>, Error>> + 'a;
    pub fn open_write<'a>(&'a mut self, key: impl AsKeyRef) -> impl Future<Output = Result<D::Writer<'a>, Error>> + 'a;
    pub fn release(self) -> Result<(), Error>;
}
impl Drop for Claim { /* self.release() (errors logged), then count down */ }

pub trait ClaimLock { /* §6 */ }
pub trait Claimer { /* §6 */ }
pub trait Store<D: IoDriver> { /* §5 */ }
```

**Async style** (D33, D80): `Vault::claim` and `Vault::shutdown` are `async fn`; `Claim`'s
object methods return `impl Future` without an `async` state machine (an `async fn` there
made tasks awaiting it unspawnable); `Store` methods return the driver's
`D::Fut` (`BoxFuture` or `LocalBoxFuture`), `Claimer::try_lock` returns `BoxFuture`. A
trait using `async fn` can't be used as `dyn Trait` and can't promise its futures are `Send`.
An `async fn`'s `Send`-ness is inferred, so it can be lost silently (as the v6 `claim`, which
held a `MutexGuard` across `.await`, showed); §13 asserts it.

**Build status:** the workspace builds and 103 tests pass (`lib-vault`: 17 unit + 1 doc;
`lib-vault-memory`: 41 integration + 4 doc; `lib-vault-posix`: 1 unit + 37 integration + 2 doc;
the doc counts include each crate's README examples),
including the compile-time `Send` checks for `AsyncIoDriver`; clippy and `cargo doc` are clean; the
`Claim::list` example is `ignore`d. Known leftovers: a stale `REVIEW(nit)` comment above the
`try_lock` docs, and some doc-comment typos.

**History.** v10 replaced the single handle pair with I/O modes (`rt`, D58; renamed to I/O
drivers in `io_driver`, unsealed, by D77), removed
`ScopedStore` (claims resolve keys themselves, D36), and moved `Send`/`Sync` from `Store` to the
mode's `StorePtr` (D74). v9 made `ClaimLock::release` synchronous (it was a `BoxFuture` run with `block_on`
in `Drop` in v6–v8) and made `Claim::release` a sync method; it briefly took `&mut self`,
and now consumes the claim (`release(self)`), as the v5–v8 `async fn release(self)` did. Removed since v5: `ClaimLock::try_release`, the
`shutting_down: Arc<AtomicBool>` gate, `Claimer::try_claim` (renamed `try_lock`). Since v4:
`StoreObject`, `Claim::open`, the claimer building the `Claim`, the `negative-impl` dependency.
Since v3: `Vault::new(root)` / `new_shared`, `shutdown_timeout`, `Claim<'a>` lifetimes,
`sub_claim`, `open() -> &File`, `open_dir() -> &Dir`, `Dir`.

## 8. Filesystem backend (`FsStore` + `FsClaimer`)

The first backend, implemented in `lib-vault-posix` (D73): `FsStore` implements
`Store<AsyncIoDriver>`, and `FsClaimer` is the claimer built for it (D45, D75). It requires the app
to run on a tokio runtime (D56). `FsStore` works with any claimer (it creates its own
directories and never touches lock files); `FsClaimer` works only with an `FsStore`
(`FsClaimer::new(&store)`). §8.3 describes `FsClaimer`; with another claimer, no lock files
exist and exclusion is that claimer's.

### 8.1 Confinement: race-free, handle-relative resolution

Checking path strings (`canonicalize` + `starts_with`, or `symlink_metadata` then `open`) leaves a
TOCTOU window, because an external process can swap a component for a symlink between check and use.
Instead:

1. The store opens the root directory once and keeps the handle. Every later operation is relative to a held handle, so renaming or replacing the root path afterwards has no effect.
2. Keys already satisfy the syntax rules, the name policy and the caps (§3). The backend adds only the case-collision check (§5).
3. Resolution, **per component on both Linux and macOS** (D73): `openat(dirfd, name, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)` for directories, `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC` for files. Mount crossings are detected by comparing `st_dev` with the root's after each step. `openat2` isn't used: the lock-file walk (§8.3) visits every component anyway, so the Linux 5.6 minimum (D53) is kept but not technically required.
   - **Later**: FreeBSD 13+ `O_RESOLVE_BENEATH`; Windows `NtCreateFile` with `RootDirectory` + `FILE_FLAG_OPEN_REPARSE_POINT`.
4. After every open, `fstat` the **handle** (not the path) and enforce §8.2.

std doesn't expose `openat` and friends, so internals use `rustix` (D52). The one exception is
the macOS ACL check, which calls the system ACL functions through a small `unsafe extern` block
(D70, D73).

These are blocking syscalls behind an async API. Every store and claimer operation runs them on
tokio's blocking pool (`spawn_blocking`), so the app must run on tokio (D56); `lib-vault` and the
memory backend stay runtime-free.

### 8.2 Links and special files (D17)

| Case | Policy | Mechanism |
|---|---|---|
| Symlink anywhere in a path | Refuse | `O_NOFOLLOW` on every component; no symlink-creation API |
| Regular file with `st_nlink > 1` | Refuse | `fstat` on the opened fd |
| Hard link created after open | Residual risk, documented | data stays on the vetted inode |
| Mount / bind mount inside the vault | Refuse | `st_dev` equals the root's, checked on every handle |
| FIFO / socket / device node | Refuse | open `O_NONBLOCK` → `fstat` → require REG/DIR → clear `O_NONBLOCK` (a FIFO open would otherwise block) |
| `list` entries | Classify via `fstatat(AT_SYMLINK_NOFOLLOW)`; links, specials, reserved `.vault*` files and names that aren't valid keys are skipped and never traversed | |
| `delete` | Removes only the regular file at the key (`unlinkat` via the parent handle); directories, links and specials are left alone | |
| Existing object on `open_write` | Same handle checks as a read, done up front even though the journal defers touching the object | |
| Modes | Never set exec/setuid/setgid. Files `0600`, dirs `0700` (`0660` / `2770` in group-sharing mode, §9) | explicit `fchmod` after create, independent of umask |

Windows (later): reject reparse points (symlinks, junctions, mount points), alternate data streams
(`name:stream`) and 8.3 short names.

### 8.3 `FsClaimer`: cross-process exclusion (D16)

**Mechanism.** Options considered for coordinating unrelated processes running as different uids:

| Mechanism | Cross-uid | Auto-release on death | Needs shared writable state | Verdict |
|---|---|---|---|---|
| **`flock` on per-key lock files beside the objects** (D67) | ✔ (anyone who can `open` the lock file) | ✔ | Only the hidden lock files; tampering is caught by verify-after-lock | **Chosen** |
| `flock` on the objects' own inodes | ✔ | ✔ | No | Original plan; can't lock a key whose object doesn't exist yet (D67) |
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

**Protocol "lock-file v1"** (D16, D67; frozen once implemented: every lib-vault version must speak it). Each key component `ci` is represented by a hidden lock file `.vault-lock-ci` in its parent directory (reserved `.vault` prefix: never a key, never listed). To claim `T = c1/…/cn` under vault root `r`:

1. Walk from the root handle, opening each directory no-follow and **creating missing intermediate directories** (a lock file needs its parent directory). Open, or create, each component's lock file no-follow.
2. In strict top-down order: `lock_shared` on `.vault-lock-c1` … `.vault-lock-c(n-1)`, then `lock` (exclusive) on `.vault-lock-cn`.
3. **Verify after lock** at each level: `fstatat(parent_fd, name, AT_SYMLINK_NOFOLLOW)` must match the locked fd's `(st_dev, st_ino)`, and `st_nlink > 0`. On a mismatch (renamed or removed while waiting), unlock that level and retry, up to `max_verify_retries` per level (default 3); when exhausted, the claim fails with `PosixError::VerifyRetriesExhausted` (D66).
4. The claim holds n lock fds (ancestors shared, target exclusive) for its lifetime. Closing them releases the locks. On release, the target's lock file is unlinked while still held exclusively; ancestor lock files stay (empty, hidden, reused).

Correctness:
- `a/b/c` vs `a/b/d`: different target lock files, shared ancestors, so they are compatible.
- `a/b` vs held `a/b/c`: the exclusive lock on `.vault-lock-b` is blocked by the holder's shared lock on it ✔. `a/b/c/d` vs held `a/b/c`: the shared lock on `.vault-lock-c` is blocked by the holder's exclusive lock ✔.
- Fixed top-down order prevents deadlock between claimants.
- **Aliasing**: on a case-insensitive filesystem `.vault-lock-Notes` and `.vault-lock-notes` are the same file, so they get the same lock. Hard links are refused. If an outsider renames or deletes a lock file, verify-after-lock (step 3) catches it and the claimant retries.

Rules and consequences:
- **Create on write** (D67): the claim locks the key's lock file, so the object doesn't need to exist. It's created by the first `open_write`; claiming and releasing a missing key leaves no object (only intermediate directories, which aren't objects). **File claims** are allowed: the exclusive lock is on that key's lock file only, not its parent directory (D59).
- **Deleting a claimed object** happens via the held parent fd and doesn't touch the lock file, so the claim stays held.
- **Permissions**: every participant needs `rwx` on the directories along its claim paths (to create lock files and directories) and read access to the lock files. Otherwise the claim fails with `PermissionDenied`.
- **Nested vault roots are not allowed** (D72): a vault root must not sit inside another vault's tree, or contain one. This is documented, not detected; if two vaults do nest, exclusion between them isn't guaranteed. No locks are taken above the root. `/` and system directories are refused as roots.
- **No in-process registry** (D73): `flock` locks belong to the open file description, so claims in the same process already exclude each other through separate opens. A registry for faster conflict errors remains optional future work.
- **Long names**: a key component can be 255 bytes, so `.vault-lock-<name>` / `.vault-journal-<name>` could exceed the 255-byte file-name limit. Those become `.vault-lock-#<hash>` / `.vault-journal-#<hash>`, hashing the ASCII-lowercased name: case-aliased names still share a file, and a collision can only over-exclude (D73).
- **Acquisition** (D18, D44): each `try_lock` is one non-blocking pass: `try_lock_shared`/`try_lock` at every level, and on `WouldBlock` release everything taken and fail with a conflict. It never parks a thread in blocking `flock(2)`; it runs on tokio's blocking pool. Callers that want to wait retry with backoff (about 1 ms → 50 ms with jitter) and their own timeout.
- **Fairness**: `flock` has no queueing, so waiters can starve.
- **Filesystems** (D69): local filesystems are expected, since NFS `flock` emulation is unreliable across hosts. Construction detects others with `statfs` (`f_type` magic numbers on Linux, `MNT_LOCAL` on macOS), logs a `tracing` warning and continues.
- **Writes and crash safety** (D19, D63): objects are written **in place**, so the inode never changes and the claim's `flock` stays on the right file. An optional journal, **on by default** and chosen when the `FsStore` is created, makes writes crash-safe:
  1. `open_write` creates `.vault-journal-<name>` beside the object (reserved `.vault` prefix: never a key, never listed). Its header holds a magic number, the content length and a checksum, plus a completion marker.
  2. `write` appends only to the journal; the object is untouched.
  3. `close()` completes the header, fsyncs the journal and the directory (`F_FULLFSYNC` on macOS), truncates and rewrites the object in place, fsyncs it, unlinks the journal and fsyncs the directory. The commit point is `close()`.
  4. A writer dropped without `close()` never touched the object; its journal is deleted (best effort).

  With journaling off, writes truncate, write in place and fsync, and a crash can leave a partial object. Journaling costs about 2× the bytes written.
- **Leftover journal** (D63): in either mode, `open_read`/`open_write` on a key whose journal exists fail with `Error::StoreError` wrapping a posix `InterruptedWrite { key, journal }` error that names the file. Only that object is affected, and `list`/`delete` work normally. The check happens at open time; there's no scan when the store is created. Recovery is manual: copy a complete journal (valid checksum) over the object, or delete an incomplete one. The library offers no replay API.
- **No read-only fallback** (D61): if the object can't be opened read-write (for example a different uid with read-only permission), `open_write` fails with a permission error (`StoreError` wrapping `PermissionDenied`), not `AlreadyClaimed`. Because claims lock lock files (D67), this surfaces when the object is opened, not when it's claimed.
- **Windows (later)**: `LockFileEx` can't lock directories. Candidate: directory handles opened with share modes (`FILE_SHARE_READ` = shared, no sharing = exclusive).

## 9. OS-level security (filesystem backend)

The library runs in-process inside the harness, so process-wide self-sandboxing (Landlock,
seccomp) is out of scope (D4).

Checks when an `FsStore` is created:
- The root's parent is canonicalized (ancestors may be symlinks, like macOS `/var`), but the root itself is opened no-follow and must be a real directory. `/` and a fixed list of system directories (`/usr`, `/tmp`, `/var`, `/Users`, `/System`, …) are refused.
- Creation is idempotent across apps: `mkdir` tolerating `EEXIST`, then validate. No shared metadata is written.
- **Always checked** at construction: a root with extended ACLs is refused with `PosixError::ExtendedAcl` (D70); a non-local filesystem is allowed with a `tracing` warning (D69). Ownership and the sharing modes control who can read a vault.
- **Construction options** (D66): `FsStore::new(root, FsStoreOptions)`. The options use builder methods over `Default`, so new options don't break callers:
  ```rust
  pub enum Sharing { Private, Group(u32 /* gid */) }

  impl Default for FsStoreOptions { /* Private, journaling on, strict modes on, 3 retries */ }
  impl FsStoreOptions {
      pub fn sharing(self, sharing: Sharing) -> Self;
      pub fn journaling(self, on: bool) -> Self;        // D63
      pub fn strict_modes(self, on: bool) -> Self;      // D64
      pub fn max_verify_retries(self, n: u32) -> Self;  // §8.3 step 3; 0 = fail on first mismatch
  }

  let store = FsStore::new("/data/vault", FsStoreOptions::default().sharing(Sharing::Group(4242)))?;
  ```
  Backend errors are a `#[non_exhaustive] PosixError` (`InterruptedWrite`, `StrictModes`, `VerifyRetriesExhausted`, `ExtendedAcl`, …), reaching callers wrapped in `Error::ClaimError` / `Error::StoreError`.
- **Strict modes** (D64, when `strict_modes` is on): modeled on sshd's `StrictModes`. Construction walks the canonical ancestors from the root's parent up to `/`, `lstat`ing each (which needs only search permission). Each ancestor must be owned by root or the effective uid and not writable by group or others, unless it is root-owned with the sticky bit (like `/tmp`). The root itself must match its sharing mode. A violation fails construction with an error naming the directory and the reason. This protects the moment the root is opened: someone able to write an ancestor could swap the path first. With `strict_modes` off, these checks are skipped.
- **Sharing mode** (formerly D30, now an `FsStore` constructor concern):
  - private: root owned by the euid, mode `0700`, files `0600`;
  - group: root group-owned by a given gid, setgid `2770`, files `0660`. Different-uid apps need this mode. POSIX ACLs are not supported on vault objects (D62), and a root with extended ACLs is refused at construction (D70).
  - A root that doesn't match the requested mode is rejected. Nothing world-writable is ever accepted.
- *(Planned, not implemented)* Linux: emit a `tracing` warning when `fs.protected_symlinks` / `fs.protected_hardlinks` are disabled.

Deployment guide (documentation):
- Run harness instances as dedicated uids that share a vault group.
- Linux: mount the vault filesystem with `nosymfollow,nodev,nosuid,noexec`.
- macOS: a dedicated APFS volume (size-limited with a quota) mounted `nodev,nosuid,noexec`.
- Cap usage with a size-capped filesystem, project quotas or `RLIMIT_FSIZE` (D8).
- Provide AppArmor / SELinux profile examples.
- Tool subprocesses spawned by the harness can be Landlocked to the vault by the harness.

## 10. AI-harness concerns

- **Name policy (strict portable ASCII, D6)** for every key and every backend, enforced by
  `Key::validate` (§3): `[A-Za-z0-9._ -]` only, no leading `-`/space or trailing `.`/space, no
  Windows device names, no `.vault` prefix, component ≤ 255 bytes, ≤ 32 components, ≤ 1024
  bytes. The case-collision rule is a `Store` requirement; create under a claim on the parent directory to avoid races (§5, D65). This rules out
  terminal-escape, bidi and homoglyph spoofing when names are shown to humans, and the same key
  is valid in every backend.
- **Claim DoS by the model**: no acquisition timeouts in the library (D23). The harness wraps
  `claim()` in a timeout and owns hold-time and claim-count policy.
- **Memory poisoning / persistent prompt injection**: the library can't judge content. Optional provenance and tamper detection (HMAC) is deferred to v2 (D68).
- **Dangerous-by-convention names** (`.git`, `.envrc`, `.vscode`, `Makefile`, `*.desktop`…): optional configurable denylist, deferred to v2 (D68). Never create executables.
- **Crash consistency**: the filesystem backend journals writes by default, so a crash never leaves a partial object silently; an interrupted write makes that object's opens fail until an operator resolves the journal (D63). Journaling can be turned off when the store is created.
- **fd hygiene** (fs): `O_CLOEXEC` everywhere. A child process inheriting a lock fd would keep a claim alive.
- **fd budget** (fs): each claim holds one lock fd per key component (at most `MAX_DEPTH` = 32).

## 11. Async notes

Async is the base API (D33), not a later addition. Notes carried over from the old v2 plan:
- `Store` methods return the driver's future (`BoxFuture` in `AsyncIoDriver`, `LocalBoxFuture` in
  `OwnedBufDriver`); `Claimer::try_lock` returns `BoxFuture`; inherent methods are `async fn` or
  return `impl Future` (§7, D80).
  `AsyncIoDriver` handles use the `futures` I/O traits (tokio users adapt with
  `tokio_util::compat`); `OwnedBufDriver` handles pass owned `Bytes` chunks (§4.3).
- **`Send` futures (`AsyncIoDriver`)**: claim methods take `&mut self`, so using a claim keeps
  futures `Send` (§4.2), readers/writers are `Send` (§4.3), and so are the `claim()` and
  `shutdown()` futures. A task awaiting `Claim`'s object methods is `Send + 'static`, so it can
  be spawned (D80). In `OwnedBufDriver` none of this is `Send`, by design (D74).
- **Which runtimes fit**: work-stealing runtimes use `AsyncIoDriver`; thread-per-core runtimes use
  `OwnedBufDriver` for stores, but their claimers must still lock without runtime I/O (P50).
- **No async in `release` or `Drop`**: `ClaimLock::release` is synchronous and must not block;
  a lock that needs async I/O to release runs it on its own thread or runtime (D39, §4.4).
- **Flush before unlock**: a backend whose writes complete in the background (e.g. a thread
  pool doing file I/O) must make sure in-flight writes land before the claim's lock is
  released, including on `Drop`, or exclusion is broken. Handles borrow the claim (§4.3), so
  they're dropped before the claim is released; a backend whose writer `Drop` leaves writes in
  flight must still finish them before its `ClaimLock::release` returns `Ok`.
- **Cancellation**: dropping a `claim()` future must release any partially acquired locks.
- **Blocking work**: each backend picks its runtime and documents it; `lib-vault-posix` uses the
  app's tokio runtime (`spawn_blocking`, D56).

## 12. Workspace and module layout

The repository is a Cargo workspace (D49). The root `Cargo.toml` lists the members,
declares the shared package `version` and `edition` in `[workspace.package]`, and declares all
third-party dependencies in `[workspace.dependencies]`; member crates inherit both with
`.workspace = true` (D50). `docs/`, `Cargo.lock` and `target/` are shared at the
root.

| Crate | Contents |
|---|---|
| `lib-vault` | the core API: `Vault<D>`, `Claim<D>`, `Key`, the `Store<D>` / `Claimer` / `ClaimLock` traits, the I/O drivers in `io_driver`, `Error` |
| `lib-vault-posix` | the filesystem backend (§8, D73): `FsStore` (`Store<AsyncIoDriver>`, any claimer), `FsClaimer` (`FsStore` only), `FsStoreOptions`, `Sharing`, `PosixError`; requires a tokio runtime (D56) |
| `lib-vault-memory` | the in-memory backend for tests and getting started (D51): `MemoryStore`, `MemoryClaimer`, `vault()`; integration tests in `tests/` exercise the core API against it |
| `lib-vault-sqlite` | the SQLite backend for typed values (D78, D79): `SqliteStore<D>` and its driver `SqliteDriver<D>` over an app-supplied `Dispatcher`, `SqliteReader`/`SqliteWriter`, `SqliteClaimer` and `SqliteClaimerOptions` (any store), `SqliteError`; sqlx, requires a tokio runtime; integration tests in `tests/sqlite/` against real database files |
| `lib-vault-clickhouse` | the ClickHouse backend for typed values (D82, D83): `ClickhouseStore<D>` and its driver `ClickhouseDriver<D>` over an app-supplied `Dispatcher`, `ClickhouseReader`/`ClickhouseWriter`, `ClickhouseClaimer` and `ClickhouseClaimerOptions` (Keeper via `KeeperMap`, any store), `ClickhouseError`; requires a tokio runtime; integration tests behind the `integration-tests` feature, against `tests/docker/compose.yaml` |

Modules in `lib-vault/src/`:

| Module | Contents |
|---|---|
| `lib.rs` | re-exports; `tracing` spans/events for diagnostics (D26) |
| `error.rs` | `Error` enum (D12) |
| `key.rs` | `Key`, `KeyError` |
| `vault.rs` | `Vault`, `VaultState` (shutdown flag, outstanding count, waiters) |
| `claim.rs` | `Claim`, release / `Drop` |
| `store.rs` | `Store<D>` |
| `io_driver/` | `IoDriver`, `IntoStorePtr`; `async_io.rs` (`AsyncIoDriver`, `AsyncRead`/`AsyncWrite` handles), `owned_buf.rs` (`OwnedBufDriver`, `Bytes` stream, `ObjectWrite`) |
| `claimer.rs` | `Claimer`, `ClaimLock` |

Modules in `lib-vault-posix/src/`:

| Module | Contents |
|---|---|
| `lib.rs` | re-exports, crate docs (choosing a claimer) |
| `store.rs` | `FsStore`: list, delete, open, the buffered writer and the commit |
| `claimer.rs` | `FsClaimer`: lock-file v1 with verify-after-lock (D67, D66) |
| `root.rs` | root opening and the construction checks (§9) |
| `fsops.rs` | handle-relative opens and `fstat` checks (§8.1, §8.2), reserved names, fsync |
| `journal.rs` | journal format and CRC-32 (D63) |
| `acl.rs` | extended-ACL detection (D70) |
| `options.rs`, `error.rs` | `FsStoreOptions`, `Sharing`, `PosixError` |

It depends on `rustix` (D52) and tokio (D56); neither is a dependency of the other crates.
*(Future)* `criterion` benchmarks in `benches/` for the performance harness (D57).

Modules in `lib-vault-sqlite/src/` (D78, D79):

| Module | Contents |
|---|---|
| `lib.rs` | re-exports, crate docs with a runnable example (in-memory database) and a `compile_fail` borrow check |
| `dispatch.rs` | `Dispatcher`, `DispatchError` |
| `driver.rs` | `SqliteDriver<D>` (`IoDriver`, `IntoStorePtr`) |
| `handles.rs` | `SqliteReader`, `SqliteWriter` |
| `store.rs` | `SqliteStore<D>`: pooled reads, `BEGIN IMMEDIATE` writes and deletes |
| `claimer.rs` | `SqliteClaimer`, `SqliteClaimerOptions`, the in-process held set, the lock, and the background task (row deletion, lease renewal, shutdown) |
| `error.rs` | `SqliteError` |

Modules in `lib-vault-clickhouse/src/` (D82, D83):

| Module | Contents |
|---|---|
| `lib.rs` | re-exports, crate docs with a `no_run` example and a `compile_fail` borrow check |
| `dispatch.rs`, `driver.rs`, `handles.rs`, `store.rs` | as in `lib-vault-sqlite`, over a `clickhouse::Client` |
| `claimer.rs` | `ClickhouseClaimer`, `ClickhouseClaimerOptions`, the claim steps, and the background task (releases, lease renewal, shutdown) |
| `protocol.rs` | the protocol's pure parts: lock plans, tokens, recognizing Keeper conflicts in server errors, root-path validation |
| `error.rs` | `ClickhouseError` |

## 13. Verification strategy

- **`Key`** (done, unit tests in `lib-vault/src/key.rs`): canonical keys accepted, each rejection case,
  the empty key rejected, each name-policy rule (characters, leading/trailing characters,
  reserved names in any case, `.vault`), the caps at and just over their limits, `join`
  failing with `TooDeep`/`TooLong`, join associativity, join always nests, component-wise
  `starts_with`/`strip_prefix`, `parent`/`object_name`, parse/display round trip.
- **Compile-time checks** (done for `AsyncIoDriver`): `Claim<AsyncIoDriver>: Send`,
  `Vault<AsyncIoDriver>: Send + Sync + Clone`, and `Send` futures from `Vault::claim` and
  `Vault::shutdown`; a task awaiting every `Claim` object method is `Send + 'static` (D80);
  `claim.release()` with a live reader doesn't compile (`compile_fail` doctests in
  `lib-vault-memory` and `lib-vault-sqlite`). To add: a test that an `OwnedBufDriver` store holding `!Send`
  state works through `Vault<OwnedBufDriver>`.
- **Core, against an in-memory backend**: scoped claims can't reach outside their key (D36);
  overlapping claims conflict in both directions and disjoint ones don't (D44);
  `ClaimLock::release` runs exactly once whether the claim is released, dropped, or both, and
  `release()` returns its error; a released key can be claimed again immediately (D39); shutdown
  refuses new claims, waits for outstanding and in-flight claims, finishes when the last one is
  dropped (including inside an executor and on another thread), and isn't blocked by a refused
  or cancelled claim; several tasks awaiting `shutdown` all finish.
- **Doc examples** compile (`cargo test --doc`), or are marked `ignore`.
- **Filesystem backend** (`lib-vault-posix/tests/`, 35 tests, passing on macOS):
  - done: the object semantics mirrored from the memory suite; create-on-write and lock-file
    cleanup; journal commit, dropped writers, leftover journals with and without journaling;
    conflicts in both directions, two stores on one root, and a **multi-process** test (the test
    binary re-runs itself as a claim holder; `kill -9` releases its locks); pre-planted
    symlinks (objects and directories), hard links and FIFOs refused; root checks (symlink,
    system directories, sharing mode, strict modes, a real ACL via `chmod +a`);
  - to do: a race test (a thread flips a path component between a directory and a symlink while
    another opens in a loop); the verify-after-lock retry path; bind mounts; cross-uid in a
    root-capable container; a **Linux** run;
  - CI (D81): clippy on Linux and macOS; build and tests on Linux, macOS and Windows (without
    `lib-vault-posix`) on stable; the ClickHouse integration tests on Linux; library builds on
    each crate's MSRV (1.89, and 1.94 for `lib-vault-sqlite`).
- **SQLite backend** (`lib-vault-sqlite/tests/sqlite/`, 29 tests against real database files,
  through an example app's `Dispatcher` in `app.rs`):
  - done: round trips through `try_as`, `value` and `into_value`; values land in the app's typed
    columns under full keys; migration on store creation; key forms and `None`; `NotFound`
    with the full key; `WrongType`; commit on `close()` (dropped writers, `close` without
    `put`, last `put` wins); overwriting with another type; `delete` and `delete(None)`;
    component-wise `list`; scoping; the case rule via the dispatcher's unique index; dispatcher
    errors surfaced; persistence across pools; 16 concurrent writers; the store with
    `MemoryClaimer` and the claimer with `MemoryStore`. Claims: conflicts in both directions
    within a claimer and between claimers on one file; immediate in-process release and
    lagging cross-claimer release; claim rows and live leases; refused and cancelled claims
    hold nothing; expired leases free their claims while live ones exclude; a short lease
    renewed past its length; a dropped claimer's rows deleted; `shutdown()`; exactly one
    winner among 16 racing claims; and a **multi-process** test (a child holds a claim and
    writes a value, then is killed with `kill -9`; the key frees once its lease runs out).
  - to do: a stalled-runtime test (a live claimer losing its lease) would need control over
    the background task's clock; a **Linux** run.
- **ClickHouse backend** (`lib-vault-clickhouse`; D82, D83):
  - unit, no server: lock plans, token owners, Keeper conflict recognition (`Node exists`,
    `Bad version`, `No node`), root-path and lease validation, `Send`/spawnability, an
    unreachable server.
  - integration, 30 tests against ClickHouse 26.9 with embedded Keeper
    (`tests/docker/compose.yaml`): the SQLite suite's object semantics, plus typed columns in
    `ReplacingMergeTree` tables and the `KeeperMap` case index; claims in both directions
    within and between claimers, rows in the `KeeperMap` tables, refused and cancelled claims
    holding nothing, expired leases reaped and live ones renewed, dropped and shut-down
    claimers removing their rows, a 16-way race for one key, 16 concurrent sibling claims, the
    claimer with `MemoryStore`, and a **multi-process** test (`kill -9`, then lease expiry).
  - to do: a multi-node Keeper and two ClickHouse servers sharing it; a stalled-runtime test.
- *(future)* Performance harness (D57): per-claim fd and syscall counts, acquisition latency vs key depth and contention.
