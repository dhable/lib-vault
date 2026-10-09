# lib-vault — Design Decisions

Companion to [`design.md`](design.md). Section references (§) point there.
`src/lib.rs` (API sketch v9, final) is the source of truth; where this log and the sketch disagree,
the sketch wins and this log is out of date.

## Decisions made

### Core API (final sketch, v9)
| # | Topic | Decision | Ref |
|---|---|---|---|
| D31 | Pluggable storage | Object data lives behind a `Store` trait: `list`, `delete`, `open_read`, `open_write`. The filesystem is the first backend; databases, object stores and in-memory stores are also intended. The vault no longer *is* a directory. | §2, §5 |
| D32 | Pluggable claiming | Exclusion is a separate `Claimer` trait. `try_lock(key)` (renamed from `try_claim` in v6) returns only a lock (`Box<dyn ClaimLock>`); the `Vault` builds the `Claim` from store + key + lock, so claimers outside the crate can be written and never see the store. `Vault::new(store, claimer)`. Resolves P28; supersedes D25, D30. | §2, §6 |
| D33 | Async-first, runtime-agnostic | No runtime dependency; I/O uses the `futures` traits. **Async trait methods** (`Store::*`, `Claimer::try_lock`) return `futures::future::BoxFuture` (the traits must be usable as `dyn` and promise `Send`). **Async inherent methods** (`Vault::claim`, `Vault::shutdown`, `Claim::list`/`delete`/`open_read`/`open_write`) are `async fn`, which also makes their checks run when polled rather than when called. Release is synchronous (D39). Supersedes D3 and the v2 async plan. | §2, §7, §11 |
| D34 | `Key` type | Object names are `Key`, an `Arc<str>` newtype in unix path syntax (`a/b/c`), not `Path`. **Strict canonical form**: not empty, no leading/trailing `/`, no `//`, no `.`/`..`, no NUL. One string has exactly one valid spelling. There is no root key (D48). Cheap to clone. | §3 |
| D35 | `Key::join` | Infallible, and always nests: unlike `Path::join`, the argument can never replace the base. This is what makes scoping a confinement mechanism. | §3 |
| D36 | Scoping | `Claim::new` wraps the vault's store in `ScopedStore::new(key, store)`, so every key a claim receives is relative to the claimed key and can't leave it. Resolves P27. | §4.1 |
| D37 | Claim thread model | `Claim` is `Send` (inferred: no `unsafe impl`, no `negative-impl` dependency). Exclusive use comes from every operation taking **`&mut self`**, and `&mut Claim` is `Send`, so tasks using a claim stay spawnable. `Claim` happens to be `!Sync` (`dyn ClaimLock` isn't `Sync`), but nothing depends on it: a `&Claim` can do nothing. Never `Clone`. Supersedes D27; resolves P18, P32, P33. | §4.2 |
| D38 | Claim operations | `list`, `delete`, `open_read` (→ `ObjectReader<'a>`), `open_write` (→ `ObjectWriter<'a>`) on keys within the claim. Separate read and write handles, because random-access read/write/seek doesn't map onto object stores. Handles are `pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>` (and the writer equivalent): pinned so the `futures` extension methods work for any backend reader, `Send` so tasks stay spawnable. Resolves P30. Replaces `sub_claim`, `open`/`open_dir`, `Dir`, `StoreObject` (D7, D9, D10, D21, D22). | §4.3, §5 |
| D39 | `ClaimLock` release | A claimer's lock has one method, `release(self: Box<Self>) -> Result<()>`: **synchronous and non-blocking**. Each implementation handles its own async work (its own thread or runtime); the library never runs a future in `release` or `Drop`. **`Ok` means the lock is released**, so the same key can be claimed again immediately. Called from `Claim::release` or `Claim::drop`. Supersedes the v6–v8 async `release` run with `block_on` in `Drop`. Resolves P44, P46, P47. | §4.4, §6 |
| D40 | `list` result | `Vec<Key>`, relative to the listed key; "prefix" means component prefix. `claim.list(None)` lists everything in the claim (D48). Resolves P31. | §5 |
| D11 | Giving up a claim | `Claim::release(&mut self) -> Result<()>` releases the lock and returns any error. `Drop` calls the same `release` if it wasn't called and logs errors with `tracing`. The lock is an `Option<Box<dyn ClaimLock>>` that is `take()`n, so it's released at most once, and a lock-less claim (failed or cancelled `try_lock`, D47) skips it. The claim still counts as outstanding until dropped. Resolves P48. | §4.4 |
| D41 | Shutdown state | One `Arc<Mutex<VaultState { shutting_down, outstanding_claims, waiters: Vec<Waker> }>>` shared by every `Vault` clone and every `Claim` replaces the `AtomicBool` gate, so the flag and the count can't be observed out of step. `claim` checks the flag and counts up under the mutex, then drops the guard **before** awaiting `try_lock` (so its future is `Send` and can't block other threads on the mutex); counting up first makes shutdown wait for in-flight claims. `Claim::drop` releases the lock if still held, counts down, and wakes the waiters, after unlocking, when the count reaches zero. `shutdown` sets the flag and awaits a `poll_fn` that is `Ready` at zero and otherwise registers its waker (deduplicated with `will_wake`). Resolves P35, P42, P43. | §4.5 |
| D47 | In-flight claim accounting | `Vault::claim` builds the `Claim` (lock-less) under the mutex right after counting up, awaits `try_lock`, then `set_lock`s it. If `try_lock` fails or the `claim` future is dropped, the lock-less `Claim` is dropped and its `Drop` counts down, so refused and cancelled claims never leak a count. Resolves P45. | §4.5 |
| D48 | No root key | The empty string is not a valid `Key` (`KeyError::Empty`); `Key::root`/`is_root` are gone, so the whole vault can't be claimed. A claim refers to its own key with `None`: `Claim::list`, `delete`, `open_read` and `open_write` take `key: impl AsKeyRef`, implemented for `&str` (validated with `Key::new`; invalid → `Error::Key`), `&Key` and `Option<&Key>`, where `None` means the claimed key itself. No impl for owned `Key` (pass `&key`), and a single `Option` impl so a bare `None` infers. `Store` still takes `&Key`, so backends always receive a full, non-empty key; `Claim` resolves `None`/`Some` via `ScopedStore::resolve`. Consequences: `join` always nests (`a` + `b` = `a/b`); `strip_prefix` of a key by itself is `None`; `parent` of a single-component key is `None`. Resolves P6. | §3, §4.1 |
| D42 | Handle lifetimes | `open_read`/`open_write` take `&'a mut self` and return handles borrowing `'a`, so a claim can't be released or dropped while a handle is alive (compile error E0505), and a claim has at most one open handle at a time, with no `list`/`delete` meanwhile. Accepted. Resolves P29. | §4.3 |
| D43 | Object semantics | `delete` of a missing key succeeds (like HTTP `DELETE`); `open_read` of a missing key is an error; `open_write` creates or overwrites. When written data is committed/flushed, and what happens to in-flight writes after release, is documented by each `Store`. `list` has no paging or limits in the first revision. | §5 |
| D44 | Claim conflicts | Documented on `Claimer`: a claim covers its key and everything beneath it (component-wise, `Key::starts_with`); conflicts go both ways (descendants *and* ancestors); disjoint keys don't conflict; exclusion holds across tasks, `Vault`s and processes. `try_lock` **never waits**: one attempt, an error on conflict; callers retry with their own backoff/timeout. Dropping the future releases anything partially acquired. Resolves P36, P40; makes P19 moot. | §6 |
| D45 | Store/Claimer pairing | Not checked by `Vault::new`. Each claimer documents which stores it works with (e.g. `FlockClaimer` only with `FsStore`). Resolves P37. | §6 |
| D46 | What shutdown waits for | Outstanding claims only, not other `Vault` clones. Several clones may await `shutdown` at once. A task that holds a claim and awaits `shutdown` never finishes (document it). Resolves P2. | §4.5 |
| D12 | Error type | A `thiserror` enum, `#[non_exhaustive]` so variants can be added without a breaking change. `ShuttingDown`: `Vault::claim` after `shutdown` started. `Key(KeyError)`: an invalid key (`From<KeyError>`, so `?` works on `Key::new`). `AlreadyClaimed { key }`: `try_lock` refused because of a conflicting claim (D44). `NotFound { key }`: no object at the key (e.g. `open_read`, D43). `ClaimError` / `StoreError`: any other claimer or store failure, wrapping a `Box<dyn Error + Send + Sync>` source so third-party implementations can pass their own errors through. Chosen over snafu: other crates construct these errors, and thiserror adds no types to the public API. Resolves P4. | §7 |
| D13 | Shutdown semantics | Stop issuing new claims, then wait until all outstanding claims are dropped (and therefore released, D39). `async fn shutdown(self)`. | §4.5 |
| D26 | Diagnostics | Use the `tracing` crate for debug info and non-fatal warnings. No warning structs are returned from the API. | §12 |
| D2 | Claim modes | Exclusive claims only. No shared/read claims. | §6 |
| D20 | Compile-time scoping | Treated as protection against harness bugs, not a security boundary. Security lives in key validation and backend resolution. | §1 |
| D8 | Quotas | Not enforced in-library; deferred to the backend / OS. | §9 |
| D4 | Deployment model | In-process library only. No process-wide self-sandboxing. | §9 |
| D5 | Multiple applications | Apps may run as different uids/gids, with no shared config or agreement beyond "same backend + lib-vault". | §6 |

### Filesystem backend (`FsStore` + `FlockClaimer`)
These were core decisions when the vault was filesystem-only. They now describe the first backend.

| # | Topic | Decision | Ref |
|---|---|---|---|
| D1 | Syscalls | The core API uses only std and `futures` types. The fs backend may use a syscall crate for `openat2`/`openat`/`fstatat`/`unlinkat`/`renameat`, with its own resolver so link, type and name policies stay ours. | §8.1 |
| D6 | Name policy | Strict portable ASCII, with reserved-name, length and case-collision rules. Enforced by the fs backend on top of `Key`'s syntax rules (whether to move it into `Key`: P38). | §10 |
| D16 | Cross-process lock mechanism | `flock` on the vault's own directory/file inodes ("inode-lock v1"): shared locks on ancestors, an exclusive lock on the target, top-down, with verify-after-lock. No lock-file directory and no shared writable metadata. | §8.3 |
| D17 | Links and special files | Symlinks, multiply-linked regular files, mount crossings, FIFOs/sockets/devices are all refused, with checks done on handles, not paths. | §8.2 |
| D18 | Non-blocking acquisition | Each `try_lock` is one pass of `try_lock_shared`/`try_lock`; on `WouldBlock` it releases what it took and fails. Never blocks in `flock(2)`. Waiting is the caller's retry loop (D44). | §8.3 |
| D19 | Claimed files | No atomic rename-replace of a claimed file (it would split the lock); write in place. | §8.3 |
| D23 | Timeouts | No built-in claim timeout or wait. Callers that retry `claim()` bound the loop with their own timeout. | §6 |
| D24 | Platforms | **Linux and macOS** first; BSD and Windows later. macOS resolves per component with `openat(O_NOFOLLOW)` plus `O_NOFOLLOW_ANY` for leaf opens; Linux uses `openat2` with a per-component fallback. | §8.1 |
| D28 | Above-root ancestor locks | Best-effort shared locks on ancestors above the root, per claim. Measure cost with a future performance harness. | §8.3 |
| D29 | What release does (fs) | Release every lock (target, then ancestors bottom-up), close fds, update the in-process registry. **No implicit `sync_all`.** For `flock` this is just closing fds: synchronous, non-blocking, and done when `release` returns. | §4.4 |

### Superseded
| # | Was | Superseded by |
|---|---|---|
| D3 | v1 sync-only; async deferred to v2 | D33 |
| D7 | `sub_claim(&mut self)` scoped borrow | D38 (no sub-claims; a claim covers its subtree) |
| D9 | `open_dir(&self) -> &Dir` | D38 |
| D10 | `sub_claim` returns `Result<Claim<'_>>` | D38 |
| D14 | `Vault(Arc<VaultInner>)`, `Claim<'static>` holding an `Arc` | D32; the refcount idea lives on in P35 |
| D15 | Shutdown via `Drop for VaultInner` + completion signal | D13, D41; waiting mechanism reopened as P35 |
| D21 | `open` returns std `File` | D38 |
| D22 | `open(&self) -> &File` | D38 |
| D25 | `Vault::new(root)` only constructor | D32 |
| D27 | `Claim` is `!Send` | D37 |
| D30 | `Vault::new` / `Vault::new_shared(root, gid)` | D32; sharing mode becomes an `FsStore` constructor concern |
| v4 `StoreObject` | `open` → `Box<dyn StoreObject>` (`AsyncRead + AsyncSeek + AsyncWrite`) | D38 (`open_read` / `open_write`) |
| v4 `Claimer::try_claim(store, key) -> Claim` | Claimer builds the claim | D32 |
| v5 `ClaimLock::try_release` | Best-effort release from `Drop` | D39 (one synchronous `release`) |
| v6–v8 async `ClaimLock::release` | `BoxFuture` release; `Drop` ran it with `futures::executor::block_on` (panicked inside a `futures` executor) | D39 (synchronous, non-blocking release) |
| v5–v8 `Claim::release` | `async fn release(self)` that consumed the claim and awaited the lock's release | D11 (sync `release(&mut self)`) |
| v4–v9 `type Error = Box<dyn Error (+ Send + Sync)>` | Placeholder error alias | D12 (`thiserror` enum) |
| v5 `shutting_down: Arc<AtomicBool>` | Shutdown gate as an atomic, re-checked after the lock was granted | D41 (one mutex over flag + count) |

## Decisions still needed

### API
| # | Question | Recommendation |
|---|---|---|
| P49 | After `Claim::release(&mut self)` the claim object still exists without a lock: `list`/`delete`/`open_*` still work on its scoped store but are no longer exclusive, and it still counts as outstanding until dropped, so `shutdown` waits for the drop. | Accepted in the final sketch. Document "drop the claim right after `release`"; an implementation could make the methods fail once the lock is gone, or `release` could consume `self`. |
| P38 | Should `Key` enforce the portable name policy (D6) for every backend, or only the fs backend? | Undecided. |

Known leftovers in the final sketch (not decisions): a stale `REVIEW(nit)` comment above the
`try_lock` docs (the dead link and unused `executor` feature it mentions are both fixed), and
doc-comment typos. The `ClaimLock` docs don't say that `release` may run on any thread, outside
any runtime, and must not block or panic (`design.md` §4.4).

### Filesystem backend
| # | Question | Options |
|---|---|---|
| P1 | Is a missing target created on claim, and as a file or directory? inode-lock v1 needs the target to exist. | create as dir by default vs a kind argument on the fs claimer |
| P8 | Allow file-level claims? | keep (with §8.3 file rules) vs directory-only |
| P9 | Ancestor-lock strictness above the root when an ancestor can't be opened | skip silently / warn / hard error / configurable |
| P10 | ACL support | POSIX ACLs in scope or not |
| P11 | Optional hardening | dangerous-name denylist, HMAC tamper detection, audit log |
| P12 | Syscall crate | `rustix` (recommended) vs raw FFI vs `cap-std` |
| P20 | Case-collision check race (runs under a shared parent lock) | accept and document vs exclusive parent lock for creates |
| P21 | Crash safety for in-place writes (D19) | recommend directory claims + atomic write helper |
| P22 | Safety of the root's parent directories | sshd StrictModes-style check vs root only |
| P23 | Verify-after-lock retry limit | count and error |
| P24 | Read-only fallback for file claims | keep vs fail with `PermissionDenied` |
| P13 | Minimum Linux kernel | require `openat2` (5.6+) vs keep fallback |
| P14 | MSRV | ≥ 1.89 (`File::lock`) |
| P25 | macOS minimum | 11+ (`O_NOFOLLOW_ANY`) vs per-component only |
| P26 | Performance harness (D28) | `criterion` vs custom bench |
| P16 | Network filesystems | refuse vs warn |
| P39 | How async methods run blocking syscalls without a runtime dependency (D33) | a small internal thread pool vs requiring a caller-provided spawner |

### Closed
| # | Resolution |
|---|---|
| P29 | Handles borrow the claim (D42). |
| P30 | `Pin<Box<dyn AsyncRead/AsyncWrite + Send + 'a>>` aliases (D38). |
| P35 | One mutex over flag, count and waiters (D41). |
| P45 | A lock-less `Claim` is built before `try_lock`, so its `Drop` gives the count back on error or cancellation (D47). |
| P42 | `claim` drops the `MutexGuard` before awaiting `try_lock`; its future is `Send` (D41). |
| P43 | `Claim::drop` counts down and wakes the waiters at zero (D41). |
| P36 | Conflict rules documented on `Claimer` (D44). |
| P37 | Pairing documented per claimer, not checked (D45). |
| P40 | `try_lock` never waits (D44). |
| P41 | `try_release` removed; a single synchronous `release` (D39). |
| P44 | No `block_on` in `Drop` any more; `release` is synchronous and non-blocking (D39). |
| P47 | `Ok` means the lock is released and the key can be claimed again immediately (D39). |
| P48 | Sync `Claim::release(&mut self)` returns the error; `Drop` logs it otherwise (D11). |
| P46 | Same: nothing is run on an executor in `Drop`, so it can't nest (D39). |
| P2 | Shutdown waits for claims only (D46). |
| P19 | Moot: the library never waits on a claim (D44). Callers that retry should back off. |
| P27 | Claims are scoped via `ScopedStore` (D36). |
| P28 | Claimer returns a `ClaimLock`; Vault builds the Claim (D32). |
| P31 | `list` returns `Vec<Key>` (D40). |
| P32 | `unsafe impl Send` and `negative-impl` removed (D37). |
| P33 | Claim operations take `&mut self` (D37). |
| P34 | `ClaimLock::release` called from `Drop`; implementations handle async work themselves (D39). |
| P4 | `thiserror` error enum (D12). |
| P6 | There is no root key; a claim names itself with `None` (D48). |
| P7 | `claim.delete(None)` deletes the claimed object itself (D48). Fs semantics in §8.3. |
| P3 | Moot: no `Dir` (D38). |
| P15 | Windows after v1 (D24). |
| P17 | Moot: sharing mode is an `FsStore` constructor concern (D32). |
| P18 | Moot: nothing depends on `Claim` being `!Sync` (D37). |
| V2-1…V2-8 | The v2 async plan is replaced by async-first (D33). Still-relevant hazards are carried in §11. |

## Alignment of `src/lib.rs` (final sketch, v9) with decisions

| Item | Status in `src/lib.rs` |
|---|---|
| Build | ✅ Builds; 10 unit tests pass; `cargo doc` is clean; the `Claim::list` example is `ignore`d; clippy is clean outside the test playground (unused variables, dead code). |
| D34, D35, D48 `Key` | ✅ Implemented with unit tests; empty keys rejected. |
| D36 scoping | ✅ `Claim::new` wraps the store in `ScopedStore`. |
| D37 thread model | ✅ `&mut self` methods; compile-time `Claim: Send` test. No `Send` assertion for the `claim()`/`shutdown()` futures (they are `Send`, checked with a probe against v8). |
| D38, D42 handles | ✅ Public pinned, `Send`, claim-borrowing aliases. |
| D43 object semantics | ✅ Documented on `Claim` and on `Store`. |
| D39, D11 release | ✅ Sync `ClaimLock::release` with a documented `Ok` contract; sync `Claim::release(&mut self)` returns errors; `Drop` releases if needed and logs errors. Claims after `release` keep working without a lock (P49). |
| D44, D45 `Claimer` | ✅ Documented. |
| D48 optional keys | ✅ `Claim::list/delete/open_read/open_write` take `impl AsKeyRef` (`&str`, `&Key`, `Option<&Key>`); `Store` takes `&Key`. |
| D41, D46, D47 shutdown | ✅ Guard dropped before `.await`; refused/cancelled claims give their count back; `Drop` counts down and wakes waiters; shutting down returns `Error::ShuttingDown`. |
| D12 errors | ✅ `thiserror` enum. |
