# lib-vault — Design

Status: **draft / planning**. v1 platforms: **Linux and macOS** (D24). BSD and Windows come later.
Release scope: **v1 is a synchronous API only.** Async support (`AsyncVault`, tokio file handles) is planned for v2 (§10).
Decision log and open questions: [`decisions.md`](decisions.md).

## Purpose

`lib-vault` gives an AI harness file-system storage for memory context, scratchpads, artifacts and
similar data, rooted at a single "vault" directory. Goals:

- **Confinement**: nothing outside the vault root is reachable through the library.
- **Claims**: every directory or file access goes through a `Claim`, a non-cloneable lock on a
  subtree. A claim is obtained only from the vault, or by transferring ownership of an existing one.
- **Exclusion across applications**: overlapping claims are refused across processes. This includes
  unrelated applications running as different uids/gids that happen to pick the same directory.
- **No links**: symbolic and hard links are never created, followed, or opened.
- **Defense in depth**: use OS and kernel mechanisms wherever they help.
- **std at the boundary**: callers manipulate data with standard Rust types (`File`, `Read`/`Write`/`Seek`).

---

## 1. Threat model

| Actor | Capability | Defended? |
|---|---|---|
| AI model via harness (possibly prompt-injected) | Chooses paths, names, content, claim patterns | **Yes, primary adversary** |
| Cooperating app (any uid) using lib-vault | Follows the lock protocol | Yes: claim exclusion |
| Same-uid process not using lib-vault | Can create links, rename, ignore locks | Partially: we never follow its links or escape the root; we can't stop its writes |
| Participant with read access acting maliciously | Can hold locks forever | **No**: advisory-lock DoS is inherent (documented) |
| Other uid without access, or root | — | Only via permissions / OS configuration |

**Compile-time scoping is not a security boundary.** Lifetimes (handles that can't outlive their
claim) protect against harness *programming bugs*. Rust code holding a `&File` can still
`try_clone()` it or call `File::unlock()`. That's acceptable because the adversary is the model,
which controls paths and content, not Rust code. Security enforcement lives in path parsing,
resolution, link and type checks, and creation modes (§2, §3, §6).

## 2. Confinement: race-free, handle-relative resolution

Checking path strings (`canonicalize` + `starts_with`, or `symlink_metadata` then `open`) leaves a
TOCTOU window, because an external process can swap a component for a symlink between check and use.
Instead:

1. `Vault::new` opens the root directory once and keeps the handle. Every later operation is relative to a held handle, so renaming or replacing the root path afterwards has no effect.
2. Caller paths are parsed into a validated `VaultPath`: relative, component-wise, no `..`/`.`/empty/NUL/prefix/root components, plus the name policy (§6).
3. Resolution:
   - **Linux ≥ 5.6 (v1)**: `openat2(dirfd, p, RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)`. On older kernels, fall back to per-component `openat(…, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)`.
   - **macOS (v1)**: per-component `openat(…, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)`. Leaf opens add `O_NOFOLLOW_ANY` (macOS 11+, P25). There is no `RESOLVE_NO_XDEV` equivalent, so mount crossings are detected by comparing `st_dev` after each step.
   - **Later**: FreeBSD 13+ `O_RESOLVE_BENEATH`; Windows `NtCreateFile` with `RootDirectory` + `FILE_FLAG_OPEN_REPARSE_POINT`.
4. After every open, `fstat` the **handle** (not the path) and enforce §3.

The lock protocol (§4) needs a handle per path component anyway, so the per-component walk is the
primary path. `openat2` is used for one-shot leaf opens inside a held claim directory.

std does not expose `openat`/`openat2`/`O_NOFOLLOW_ANY`, so internals use a thin syscall crate
(`rustix`). The public API exposes only std types.

## 3. Links and special files

| Case | Policy | Mechanism |
|---|---|---|
| Symlink anywhere in a path | Refuse | resolve flags / `O_NOFOLLOW`; no symlink-creation API |
| Regular file with `st_nlink > 1` | Refuse | `fstat` on the opened fd |
| Hard link created after open | Residual risk, documented | data stays on the vetted inode |
| Mount / bind mount inside the vault | Refuse | `RESOLVE_NO_XDEV` / `st_dev` equals the root's |
| FIFO / socket / device node | Refuse | open `O_NONBLOCK` → `fstat` → require REG/DIR → clear `O_NONBLOCK` (a FIFO open would otherwise block) |
| `read_dir` entries | Classify via `fstatat(AT_SYMLINK_NOFOLLOW)`; links and specials are reported as `Unsupported` and never traversed | |
| `remove_dir_all` | Handle-relative walk + `unlinkat`; never follows | |
| Modes | Never set exec/setuid/setgid. Files `0600`, dirs `0700` (`0660` / `2770` in group-sharing mode, §5) | explicit `fchmod` after create, independent of umask |

Windows (later): reject reparse points (symlinks, junctions, mount points), alternate data streams
(`name:stream`) and 8.3 short names.

## 4. Claims and cross-process exclusion

### 4.1 Mechanism: `flock` on the vault's own inodes

Mechanisms considered for coordinating unrelated processes running as different uids:

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

The kernel's `flock` table is global and keyed by inode. Uid doesn't matter, only the ability to open
the object. std exposes it directly: `File::open` works on Unix directories, and
`File::lock`/`lock_shared`/`try_lock`/`try_lock_shared`/`unlock` (Rust 1.89+) map to `flock` on Unix.
Locks belong to the open file description, so separate opens within one process also exclude each other.

### 4.2 Protocol "inode-lock v1" (frozen; every lib-vault version must speak it)

To claim target `T = r/c1/…/cn` (`r` = vault root):

1. Walk from the root handle, opening each component no-follow.
2. In strict top-down order: `lock_shared` on `r` and every ancestor, then `lock` (exclusive) on `T`.
3. **Verify after lock** at each level: `fstatat(parent_fd, name, AT_SYMLINK_NOFOLLOW)` must match the locked fd's `(st_dev, st_ino)`, and `st_nlink > 0`. On a mismatch (renamed or removed while waiting), unlock that level and retry, with a bounded retry count.
4. The claim holds n+1 fds (ancestors shared + target exclusive) for its lifetime. Closing them releases the locks.

Correctness:
- `a/b/c` vs `a/b/d`: both share only ancestors, so they are compatible.
- `a/b` vs held `a/b/c`: the exclusive lock on `a/b` is blocked by the holder's shared lock ✔. `a/b/c/d` vs held `a/b/c`: the shared lock on `a/b/c` is blocked by the holder's exclusive lock ✔.
- Fixed top-down order prevents deadlock between claimants.
- **Aliasing is solved for free**: `Notes` and `notes` on a case-insensitive filesystem are the same inode, so they get the same lock. Hard links are refused. A rename by an outsider moves the lock with the inode.

Rules and consequences:
- **The target must exist** before it can be locked, so claiming a missing path must create it. How the kind (dir/file) is chosen is an open decision.
- **Deleting a claimed target** happens via the held parent fd. Waiters then wake on a dead inode, fail verify and retry.
- **Read permission is required**: opening a directory for `flock` needs `r`, so every participant needs `r-x` along its claim paths. Otherwise the claim fails with `PermissionDenied`.
- **Nested vault roots** (app A at `/data/v`, app B at `/data/v/sub`): also take **best-effort shared locks on every ancestor above the vault root up to `/`**. Unopenable ancestors are skipped, with reduced assurance and a `tracing` warning. `/` and system directories are refused as roots. These locks are taken per claim; their cost is deferred to the performance harness (D28).
- **Same-thread self-deadlock**: an in-process registry keyed by `(dev, ino)` detects a blocking claim that overlaps one held by the same thread, and gives fast conflict errors. `flock` stays the source of truth.
- **Fairness**: `flock` has no queueing, so waiters can starve.
- **Filesystems**: local filesystems only for v1, since NFS `flock` emulation and directory locking are unreliable. Detect this with `statfs`: `f_type` magic numbers on Linux, the `MNT_LOCAL` flag on macOS.
- **Windows (later)**: `LockFileEx` can't lock directories. Candidate: directory handles opened with share modes (`FILE_SHARE_READ` = shared, no sharing = exclusive), which are kernel-enforced.

### 4.3 Acquisition

- A single internal routine, `acquire(path, Mode::{Try, Deadline(Instant), Forever}, cancel)`, calls `try_lock*` at each level.
  - On `WouldBlock`, `Try` releases everything it took and returns a conflict error.
  - v1 exposes only `Try` (`try_claim`) and `Forever` (`claim`). `Deadline` stays internal until v2 (D23).
  - The other modes back off (about 1 ms → 50 ms with jitter) and re-check `cancel` and the vault's shutdown flag.
- Blocking `claim` also polls rather than calling blocking `File::lock`, because a thread parked inside `flock(2)` can't be woken by `shutdown`.
- The `cancel` flag is unused in v1, but it's kept so a v2 async wrapper can run `acquire` on a helper thread and cancel it when the future is dropped.

### 4.4 File claims

1. **No atomic replace of a claimed file.** Temp-file + rename creates a new inode: the holder keeps locking the orphaned old one while a waiter locks the new one, and both believe they hold the claim. File claims are written in place (`set_len(0)` + write + `sync_all`). Atomic replace is offered only through a parent directory claim's `Dir`. That's safe because nobody can hold claims beneath an exclusively locked directory.
2. **Open mode.** Open `O_RDWR`, and fall back to `O_RDONLY` when a cross-uid participant has only read permission. In that case writes fail with `PermissionDenied`.
3. Calling `File::unlock()` on the returned handle releases the claim's own lock. Document this as a harness-bug hazard.

### 4.5 Sub-claims

- `sub_claim(&mut self, path)` validates the path, resolves it no-follow relative to the parent's directory fd, and `fstat`-checks it.
- **No locks are taken**: the parent's exclusive lock covers the subtree.
- The mutable borrow makes the parent unusable while the sub-claim lives.
- An empty path or `"."` is rejected. If the parent is a file, the call fails with "not a directory".

### 4.6 No moving claims (D27)
- `Claim` is `!Send` (via `negative-impl`), so a claim stays on the thread that acquired it. Ownership can still move between owners on that thread. `!Sync` is recommended too (P18).
- `Claim` never implements `Clone`.
- There's no API to move or copy files between two claims; renames stay inside one claim.
- Threads share a `Vault` (`Clone + Send + Sync`) and each acquires its own claims.
- A bonus: because a claim never changes thread, the in-process registry can key holders by thread id. A blocking `claim` that overlaps one held by the calling thread fails fast instead of deadlocking, and so does `shutdown` called from a thread that holds a claim.

## 5. OS-level security (in-process library scope)

The library runs in-process inside the harness, so process-wide self-sandboxing (Landlock, seccomp)
is out of scope for the library itself.

Checks in `Vault::new`:
- The root is a real directory (opened no-follow), on a local filesystem, and not `/` or a system directory.
- `new` is idempotent across apps: `mkdir` tolerating `EEXIST`, then validate. It writes no shared metadata, because inode-lock v1 needs none.
- **Sharing mode** (chosen by constructor, D30):
  - `Vault::new(root)`, private: root owned by the euid, mode `0700`, files `0600`.
  - `Vault::new_shared(root, gid)`, group: root group-owned by `gid`, setgid `2770`, files `0660`. Different-uid apps need this mode or admin-configured ACLs.
  - A root that doesn't match the requested mode is rejected.
  - Nothing world-writable is ever accepted.
- Linux: emit a `tracing` warning when `fs.protected_symlinks` / `fs.protected_hardlinks` are disabled. macOS has no equivalent sysctls.

Deployment guide (documentation):
- Run harness instances as dedicated uids that share a vault group.
- Linux: mount the vault filesystem with `nosymfollow,nodev,nosuid,noexec`. `nosymfollow` (Linux 5.10+) makes the kernel refuse symlinks.
- macOS: a dedicated APFS volume (size-limited with a quota) mounted `nodev,nosuid,noexec`.
- Cap usage with a size-capped filesystem, project quotas or `RLIMIT_FSIZE` (quotas are not enforced in-library).
- Provide AppArmor / SELinux profile examples.
- Tool subprocesses spawned by the harness can be Landlocked to the vault by the harness.

Future platforms: OpenBSD `unveil`, FreeBSD Capsicum, macOS sandbox profiles, Windows ACLs / AppContainer.

## 6. AI-harness concerns

- **Name policy (strict portable ASCII)**: components match `[A-Za-z0-9._ -]+`.
  - No leading `-` or space, no trailing `.` or space, and not `.`/`..`.
  - No Windows reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`, with any extension).
  - Component ≤ 255 bytes; path depth and length are capped.
  - The `.vault` prefix is reserved.
  - **Case-collision check** on create: creating `Foo` is refused when `foo` exists.
  - This rules out terminal-escape, bidi and homoglyph spoofing when names are shown to humans.
- **Claim DoS by the model**: v1 has no acquisition timeouts (D23). The harness should prefer `try_claim`, and owns hold-time and claim-count policy.
- **Memory poisoning / persistent prompt injection**: the library can't judge content. Future work: optional provenance and tamper detection (HMAC sidecar).
- **Dangerous-by-convention names** (`.git`, `.envrc`, `.vscode`, `Makefile`, `*.desktop`…): optional configurable denylist. Never create executables; recommend `noexec`.
- **Crash consistency**: `write_atomic` (temp in the same dir → `fsync` → `renameat` → fsync dir), only via `Dir` (see §4.4).
- **fd hygiene**: `O_CLOEXEC` everywhere (std default). A child process inheriting a lock fd would keep a claim alive.
- **fd budget**: each claim holds depth+1 fds, plus ancestors above the root. Cap the path depth.

## 7. Public API

Current interface: `src/lib.rs`. The target v1 shape (sync only), incorporating the decisions in `decisions.md`:

```rust
#[derive(Clone)]
pub struct Vault(Arc<VaultInner>);                       // Clone + Send + Sync: shared across threads
impl Vault {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, Error>;                  // private mode (D30)
    pub fn new_shared(root: impl AsRef<Path>, gid: u32) -> Result<Self, Error>; // group mode (D30)
    pub fn try_claim(&self, p: impl AsRef<Path>) -> Result<Claim<'static>, Error>;
    pub fn claim(&self, p: impl AsRef<Path>) -> Result<Claim<'static>, Error>;            // blocks (polls)
    pub fn shutdown(self);                                // gate, drop own Arc, wait for completion
    pub fn shutdown_timeout(self, t: Duration) -> Result<(), Error>;
}

pub struct Claim<'a> { inner: Option<ClaimInner>, _marker: PhantomData<&'a mut ()> }   // never Clone (D27)
#[negative_impl] impl !Send for Claim<'_> {}               // D27; plus !Sync per P18
struct ClaimInner { vault: Arc<VaultInner>, target: Target, locks: Option<LockSet> /* None for sub-claims */ }
enum Target { Dir(Dir), File(File) }                       // the locked fd IS the handle returned

impl<'a> Claim<'a> {
    pub fn sub_claim(&mut self, p: impl AsRef<Path>) -> Result<Claim<'_>, Error>;
    pub fn open(&self) -> Result<&File, Error>;           // D22: &File implements Read/Write/Seek
    pub fn open_dir(&self) -> Result<&Dir, Error>;
    pub fn release(mut self) -> Result<(), Error> { self.release_impl() }
    fn release_impl(&mut self) -> Result<(), Error>;      // idempotent via self.inner.take()
}
impl Drop for Claim<'_> { fn drop(&mut self) { let _ = self.release_impl(); } }

pub struct Dir { /* OwnedFd + policy */ }   // our wrapper; mirrors the expected std::fs::Dir method names
type Error = Box<dyn std::error::Error + Send + Sync>;    // placeholder until a concrete enum
```

### 7.1 Release and `Drop`
- `release()` and `Drop` share one private `release_impl(&mut self)`. It must be idempotent (`self.inner.take()`), because `Drop` runs again after `release()`.
- `release_impl` does (D29): unlock the target, then the ancestors bottom-up; drop the fds; update the registry. Consuming `self` ends the claim's lifetime, so every `&File` / `&Dir` borrowed from it is invalid at compile time.
- **No implicit `sync_all`.** Callers who need durability call `claim.open()?.sync_all()` before `release()`.
- `release()` is how callers observe unlock and bookkeeping errors. `Drop` is best-effort and swallows errors, like std `File`. std has no fallible `close`.
- Sub-claims hold no locks, so releasing one just returns control to the parent.

### 7.2 Shutdown (refcount + completion signal)
```rust
struct VaultInner { shutting_down: AtomicBool, done: Completion, /* root fd, registry, options */ }
impl Drop for VaultInner {      // runs once, when the LAST Arc (any Vault clone or claim) drops
    fn drop(&mut self) { /* non-blocking cleanup: close root fd, clear registry */ self.done.fire(); }
}
impl Vault {
    pub fn shutdown(self) {
        self.0.shutting_down.store(true, Release);   // every clone stops issuing claims; pollers fail
        let waiter = self.0.done.subscribe();
        drop(self);                                  // release our own Arc
        waiter.wait();                               // returns once every claim and Vault clone is gone
    }
}
```
- `Completion` is a small `Mutex<bool> + Condvar` type. v2 can add a `Vec<Waker>` for an awaitable shutdown.
- **Hazard:** `let c = vault.try_claim(..)?; vault.shutdown();` would never return, because `c` holds an Arc. Since claims are `!Send`, `shutdown` can detect claims held by the calling thread and fail instead. Claims held by other threads are simply waited for; `shutdown_timeout` bounds that wait.
- `VaultInner::drop` may run on any thread (whichever drops the last claim or `Vault` clone), so it must never block.
- Scope is this process only. Other apps sharing the vault are unaffected.

### 7.3 `Dir`
Our own wrapper, even if `std::fs::Dir` stabilizes, because std wouldn't enforce no-follow on every
component, the nlink/type checks, the name policy or creation modes. It mirrors the expected std names:
- `open_file` / `create_file`
- `open_dir`
- `create_dir` / `create_dir_all`
- `read_dir` → `EntryKind::{File, Dir, Unsupported}`
- `remove_file` / `remove_dir` / `remove_dir_all`
- `rename` (confined to this `Dir`)
- `write_atomic`
- `metadata`

### 7.4 Async callers in v1
v1 has no async API. An async harness can call `try_claim` directly (it never blocks). It should
wrap blocking `claim` and file I/O in its runtime's `spawn_blocking`. Claims are `!Send` (D27), so the
claim must be acquired, used and released *inside* the closure; a clone of the `Vault` is what moves in.

## 8. Module layout (when coding starts)

| Module | Contents |
|---|---|
| `lib.rs` | re-exports; `tracing` spans/events for diagnostics (D26) |
| `error.rs` | error type |
| `path.rs` | `VaultPath`, name policy |
| `vault.rs` | `Vault`, `VaultInner`, root checks, sharing-mode detection, shutdown / `Completion` |
| `claim.rs` | `Claim`, `ClaimInner`, `Target`, `LockSet`, release / `Drop` |
| `lock.rs` | inode-lock v1, verify-after-lock, acquisition routine, in-process registry |
| `dir.rs` | `Dir` wrapper |
| `sys/{linux,macos}.rs` | v1 resolver, type and link checks, `statfs` local-filesystem detection (`bsd`/`windows` later) |
| `benches/` *(future)* | performance harness (D28, P26) |
| *(v2)* `tokio.rs` | feature-gated `AsyncVault`, `Tokio` I/O mode |

## 9. Verification strategy

- `VaultPath` table tests: `..`, absolute paths, NUL, reserved names, trailing dot, case collisions.
- Race test: a thread flips a path component between a directory and a symlink while another thread opens in a loop. Nothing may ever escape the root.
- Pre-planted symlink, hard link, FIFO and bind mount are refused.
- Multi-process tests (re-exec the test binary as children):
  - overlapping vs disjoint claims;
  - `kill -9` of the holder releases its locks;
  - a waiter on a removed directory hits verify and retries.
- Cross-uid: a CI job in a root-capable container runs two children under different uids sharing a group-mode vault and asserts exclusion.
- Nested vault roots: an outer-root exclusive claim blocks an inner-root claim.
- Shutdown: blocks until the last claim is released; `shutdown_timeout` returns an error while a claim is held.
- CI matrix: Linux (a kernel with `openat2` plus a fallback-path run) and macOS. Cross-uid tests run on Linux only.
- *(future)* Performance harness: per-claim fd and syscall counts and acquisition latency vs path depth and contention, including the above-root ancestor locks (D28).
- *(v2)* Async: a dropped future releases partial locks; a claim dropped with in-flight tokio writes keeps all its locks until the writes land.

## 10. v2: async support (deferred)

Not part of v1. Captured here so v1 choices stay compatible.

**Constraints:**
- `tokio::fs::File` implements `AsyncRead`/`AsyncWrite`/`AsyncSeek` and `AsFd`, not std `Read`/`Write`/`Seek`/`FileExt`. So no single `VaultFile` trait can describe both sync and tokio files.
- tokio implements `AsyncWrite` for `File` but not `&File`, so handing out the file requires `&mut`.
- A method returning `&impl Trait` has one concrete type, so the file type has to come from a type parameter.
- `Drop` can't be specialized per type parameter (E0366). Per-mode drop behavior goes through a trait method.

**Preferred shape:** a sealed I/O-mode trait plus a defaulted type parameter. Adding the parameter in v2 isn't a breaking change for v1 users.
```rust
pub trait IoMode: sealed::Sealed + Send + Sync + 'static {
    type File: Send + Sync + AsFd;
    #[doc(hidden)] fn wrap(f: std::fs::File) -> Self::File;
    #[doc(hidden)] fn release_on_drop(file: Self::File, locks: private::LockSet);
}
pub enum Blocking {}                         // File = std::fs::File (v1 behavior)
#[cfg(feature = "tokio")] pub enum Tokio {}  // File = tokio::fs::File, via File::from_std

pub struct Claim<'a, M: IoMode = Blocking> { /* … */ }
enum Target<M: IoMode> { File { lock: std::fs::File, file: M::File }, Dir(Dir) }  // lock = try_clone, same open file description
impl Claim<'_, Blocking> { pub fn open(&self) -> Result<&File, Error>; }               // v1 signature, unchanged (D22)
impl<M: IoMode> Claim<'_, M> { pub fn open_mut(&mut self) -> Result<&mut M::File, Error>; }  // added in v2
impl Claim<'_, Blocking> { pub fn release(self) -> Result<(), Error>; }
#[cfg(feature = "tokio")] impl Claim<'_, Tokio> { pub async fn release(self) -> Result<(), Error>; }
#[cfg(feature = "tokio")] pub type AsyncClaim<'a> = Claim<'a, Tokio>;
```
- **The lock stays on a std handle.** It sits on a std `try_clone` of the same open file description, so locking and unlocking never need tokio APIs.
- **Exclusion hazard.** tokio writes complete on a background thread. Async `release` must `flush().await` before unlocking. `Drop` must move the file *and all locks (including ancestors)* into a spawned cleanup task (`Handle::try_current()`) that flushes before dropping them.
- **Open:** async `Dir` (an associated `type Dir` using `spawn_blocking`) vs a sync `Dir` in tokio mode.
- **Rejected:** a single file type implementing both sync and async traits (fails at runtime when used in the wrong mode), and separate `Claim`/`AsyncClaim` structs (duplicate API).
- **v1 compatibility:** v1 keeps `open(&self) -> &File` (D22). v2 adds `open_mut` for mode-generic access instead of changing `open`, so v1 callers don't break. `open` stays available only on `Claim<'_, Blocking>`.
