# lib-vault — Design Decisions

Companion to [`design.md`](design.md). Section references (§) point there.

## Decisions made

| # | Topic | Decision | Ref |
|---|---|---|---|
| D1 | "Standard Rust only" | Callers see only std types/traits (`File`, `Read`/`Write`/`Seek`, `Metadata`). Internals may use a syscall crate for `openat2`/`openat`/`fstatat`/`unlinkat`/`renameat`. We write our own resolver so the link, type and name policies stay ours. | §2 |
| D2 | Claim modes | Exclusive claims only for v1. No shared/read claims. | §4.2 |
| D3 | Non-blocking acquisition | **v1 is sync-only**: `try_claim` (fails fast) and blocking `claim` (polls). Async (`AsyncVault`, tokio file handles, async release/shutdown) is **deferred to v2**. | §4.3, §10 |
| D4 | Deployment model | In-process library only. No process-wide self-sandboxing (Landlock/seccomp). OS hardening comes from kernel resolve flags, permissions and a deployment guide. | §5 |
| D5 | Multiple applications | Apps may run as different uids/gids, with no shared config or agreement beyond "same directory + lib-vault". | §4.1 |
| D6 | Name policy | Strict portable ASCII, with reserved-name, length and case-collision rules. | §6 |
| D7 | Sub-claims | Scoped borrow: `sub_claim(&mut self, …)`; the parent is unusable while the child lives. Sub-claims take no locks. | §4.5 |
| D8 | Quotas | Not enforced in-library; deferred to OS (filesystem quotas, size-capped mount). | §5 |
| D9 | Handle access | `open_dir(&self) -> Result<&Dir>`; `open` per D22. The locked fd is the handle returned. | §7 |
| D10 | Sub-claim errors | `sub_claim` returns `Result<Claim<'_>, Error>`. | §4.5 |
| D11 | Release | `release(self) -> Result<()>` consumes the claim, which ends its lifetime and every borrowed handle. It and `Drop` share an idempotent private `release_impl(&mut self)`. What it does: see D29. | §7.1 |
| D12 | Error type | `Box<dyn Error>` alias as an acknowledged placeholder until a concrete type is designed (P4). | §7 |
| D13 | Shutdown semantics | Stop issuing new claims, then block until all outstanding claims are released. | §7.2 |
| D14 | Claim ownership | Shared ownership: `Vault(Arc<VaultInner>)`, `Clone + Send + Sync`. Top-level claims are `Claim<'static>` and hold an `Arc` clone; only sub-claims borrow. | §7 |
| D15 | Shutdown shape | `Vault::shutdown(self)` consumes. Cleanup lives in `Drop for VaultInner`, plus a gate flag and a completion signal fired when the last `Arc` drops. | §7.2 |
| D16 | Cross-process lock mechanism | `flock` on the vault's own directory/file inodes ("inode-lock v1"): shared locks on ancestors, an exclusive lock on the target, top-down, with verify-after-lock. No lock-file directory and no shared writable metadata. | §4.1–4.2 |
| D17 | Links and special files | Symlinks, multiply-linked regular files, mount crossings, FIFOs/sockets/devices are all refused, with checks done on handles, not paths. | §3 |
| D18 | Blocking acquisition | Polls `try_lock` with backoff instead of blocking in `flock(2)`, so shutdown and cancellation can interrupt it. | §4.3 |
| D19 | Claimed files | No atomic rename-replace of a claimed file (it would split the lock); write in place. Atomic replace only via a parent `Dir`. | §4.4 |
| D20 | Compile-time scoping | Treated as protection against harness bugs, not a security boundary. Security lives in path, resolution and link checks. | §1 |
| D21 | v1 file handle type | `open` returns a std `File`. No `VaultFile` trait in v1: it can't cover tokio's file (which has async traits only), and v1 has no second implementation. | §7 |
| D22 | `open` receiver | `open(&self) -> Result<&File, Error>` (std implements `Read`/`Write`/`Seek` for `&File`). **Accepted consequence:** v2's tokio file needs `&mut` to write, so v2 must either make a breaking change to `open` or add a separate method for tokio mode (e.g. `open_mut(&mut self) -> &mut M::File`). | §7, §10 |
| D23 | Timeouts | No `claim_timeout` in v1. Callers needing a deadline loop on `try_claim`. Revisit in v2. | §4.3 |
| D24 | v1 platforms | **Linux and macOS.** BSD and Windows come after v1. macOS resolves per component with `openat(O_NOFOLLOW)`, and uses `O_NOFOLLOW_ANY` (macOS 11+) for leaf opens. Linux uses `openat2` with a per-component fallback. `flock` works on directories on both. | §2, §11 |
| D25 | No `VaultOptions` | The options type is dropped; `Vault::new(root)` is the only constructor. Policy values (name rules, depth/length caps) are fixed constants in v1. How the sharing mode is chosen without options: P17. | §5, §7 |
| D26 | Diagnostics | Use the `tracing` crate (already in `Cargo.toml`) for debug info and non-fatal warnings (e.g. `protected_symlinks` off, an above-root ancestor skipped, verify-after-lock retries). No warning structs are returned from the API. | §5, §8 |
| D27 | Moving claims | **Disallowed**, in two senses. (1) `Claim` is **`!Send`** (via `negative-impl`), so a claim stays on the thread that acquired it; "transfer ownership" means moving it between owners on that thread. (2) No moving or copying files between two claims; renames stay inside one claim. `Claim` never implements `Clone`. `Vault` stays `Clone + Send + Sync`, so each thread acquires its own claims. | §4.6, §7 |
| D28 | Above-root ancestor locks | Keep per-claim best-effort shared locks as designed for v1; don't optimize now. Plan a **performance test harness** to measure per-claim cost (fds, syscalls, latency vs depth) for future exploration. | §4.2, §9 |
| D30 | Sharing mode selection | Two constructors: `Vault::new(root)` is private mode (owned by the euid, `0700`/`0600`); `Vault::new_shared(root, gid: u32)` is group mode (group-owned, setgid `2770`/`0660`). A root that doesn't match the requested mode is rejected. | §5, §7 |
| D29 | What `release` does | Releases every lock the claim holds (target, then ancestors bottom-up), closes its fds, updates the in-process registry, and ends the claim's lifetime. **No implicit `sync_all`**: durability is the caller's job, via `open()?.sync_all()` before releasing. | §7.1 |

## Decisions still needed

### API
| # | Question | Options | Recommendation |
|---|---|---|---|
| P1 | How does a claim choose the target kind (dir vs file), and is a missing target created? The locking design needs the target to exist. Also applies to `sub_claim`. | (a) existing-only plus `try_claim_new(path, Kind)`; (b) kind argument; (c) `OpenOptions`-style builder: `vault.claim(p).dir().create(true).try_acquire()` | (c), keeping `try_claim` / `claim` as "existing object" shorthands |
| P2 | Should shutdown wait for other live `Vault` clones too, or only for claims? (With claims `!Send`, a thread calling `shutdown` while holding a claim can be detected and rejected instead of hanging.) | refcount-only (waits for everything, simplest) vs a separate live-claims counter | Undecided |
| P3 | Files opened through `Dir`: owned or lifetime-bound? | owned std `File` vs `FileRef<'_>` deref'ing to `File` | Owned `File`, documented |
| P4 | Concrete error type: when, and which variants? Interim: add `+ Send + Sync` to the alias. | enum with `From<Error> for io::Error` (conflict → `WouldBlock`) vs `io::Error` only | Enum, once the implementation shows which failures occur |
| P6 | What does claiming the vault root mean? | allow `""` vs explicit `try_claim_root()` vs disallow | Explicit `try_claim_root()` |
| P7 | API to delete a claimed target itself | `claim.remove(self)` vs only via the parent's `Dir` | `remove(self)` |

### Scope and behavior
| # | Question | Options |
|---|---|---|
| P8 | Keep file-level claims in v1? | keep, with the §4.4 rules, vs directory-only claims in v1 |
| P9 | Strictness of locks on ancestors above the vault root (nested-vault safety) when an ancestor can't be opened | skip silently / warn / hard error / configurable |
| P10 | ACL support | POSIX ACLs in scope or not (sharing-mode selection moved to P17) |
| P11 | Optional hardening features for v1 | dangerous-name denylist, provenance/HMAC tamper detection, audit log: v1 or later |
| P12 | Syscall crate | `rustix` (recommended) vs raw FFI vs adopting `cap-std` |
| P18 | Should `Claim` also be `!Sync`? | If `Claim` is `Sync`, a `&Claim` can be shared with another thread, which could then call `open()` and use the `&File` there. That defeats the intent of D27. Recommendation: `!Sync` too. |
| P19 | Deadlock between two claims | Same-thread overlap is now reliably detectable because claims are `!Send` (D27): a blocking `claim` on a path overlapping a claim held *by the calling thread* fails fast. Still open is the cross-thread case: T1 holds `a/x` and blocks on `a/y` while T2 holds `a/y` and blocks on `a/x`. With no timeout (D23), only `shutdown` breaks it. Options: document "use `try_claim` while holding a claim", a wait-for-graph check in the in-process registry, or revisit timeouts. |
| P20 | Case-collision check race | The check runs under only a shared lock on the parent, so concurrent sibling creates (`Foo` / `foo`) can both pass, and it costs a directory scan per create. Accept and document, or take the parent exclusively for creates. |
| P21 | Crash safety for file claims | D19 means file claims are truncated and rewritten in place, so a crash leaves a partial file. Recommend "claim the directory and use `Dir::write_atomic`" for important files? Interacts with P8. |
| P22 | Safety of the root's parent directories | Check that the root's ancestors aren't writable by other users (sshd StrictModes-style), or only check the root itself |
| P23 | Verify-after-lock retry limit | number of retries, and the error returned when exhausted |
| P24 | Read-only fallback for file claims (§4.4) | keep for non-group setups vs fail with `PermissionDenied` (group mode files are `0660`, so the fallback rarely applies) |

### Platform and support
| # | Question | Options |
|---|---|---|
| P13 | Minimum Linux kernel | require `openat2` (5.6+) vs keep the per-component `openat` fallback |
| P14 | MSRV | ≥ 1.89 (needed for `File::lock`); pin exact version |
| P15 | ~~When to investigate Windows locking~~ | Resolved by D24: after v1 |
| P25 | macOS minimum version | macOS 11+ (needed for `O_NOFOLLOW_ANY`) vs supporting older releases with per-component walks only |
| P26 | Performance test harness (D28) | scope: per-claim fd and syscall counts, acquisition latency vs path depth and contention; tooling such as `criterion` vs a custom bench binary |
| P16 | Network filesystems | refuse vs warn on NFS/SMB at `Vault::new` |

## Deferred to v2 (async)

Not designed in detail; captured so v1 doesn't paint us into a corner.

| # | Item | Notes |
|---|---|---|
| V2-1 | `AsyncVault` (`claim().await`, `shutdown(self).await`) | Acquisition on a helper thread, completing a `Waker`; dropping the future cancels and releases partial locks. Timeouts via `tokio::time::timeout` or a built-in deadline. |
| V2-2 | Claims returning `tokio::fs::File` | Preferred shape: a sealed `IoMode` trait with an associated `File` type; `Claim<'a, M: IoMode = Blocking>`; `pub type AsyncClaim<'a> = Claim<'a, Tokio>`. Keep the flock on a std `try_clone` of the same open file. Design sketch in `design.md` §10. |
| V2-3 | Async release / `Drop` safety | tokio writes finish on a background thread. Async `release` must `flush().await` before unlocking. `Drop` must hand the file **and all locks, including ancestors** to a spawned cleanup task, so exclusion holds until in-flight writes land. Per-mode `Drop` behavior goes through a trait method (Rust can't specialize `Drop`). |
| V2-4 | Async `Dir` | Associated `type Dir` with async methods via `spawn_blocking`, or a sync `Dir` plus caller-side `spawn_blocking`. |
| V2-5 | Cargo feature | `#[cfg(feature = "tokio")]`, not `target_feature`. |
| V2-6 | `open` for tokio mode | Because of D22, add `open_mut(&mut self)` for mode-generic access (or make a breaking change to `open`). |
| V2-7 | Claim timeouts | Sync `claim_timeout` and/or async deadline (D23). |
| V2-8 | `!Send` claims vs tokio | A `!Send` claim held across an `.await` makes the future `!Send`, so it can't go to `tokio::spawn` on the multi-threaded runtime (only `spawn_local` / `LocalSet`). v2 must either keep that restriction or make `AsyncClaim` `Send`, revisiting D27 for async mode. |

## Alignment of `src/lib.rs` (API v3) with decisions

| Decision | Status in `src/lib.rs` |
|---|---|
| D9, D10 | ✅ Done |
| D11 | ✅ Structure done. `release_impl` must become idempotent once `Claim` has fields; move the "Consumes the ownership…" doc comment from `release_impl` to `release`. |
| D14 | ✅ Mostly done. `Vault(Arc<VaultInner>)` derives `Clone`, claims return `Claim<'static>` and hold an `Arc`. `VaultInner::try_claim(&self)` still can't produce that `Arc` (needs `self: &Arc<Self>`, or `Vault` passes the clone in). |
| D15 | ⚠ `shutdown` uses `Arc::get_mut`, which returns `None` (does nothing) while any claim or clone is alive, and doesn't gate new claims. Replace it with the gate flag + completion signal (§7.2). |
| D3 / D21 / D22 | ✅ Async module and `VaultFile` trait removed. ❌ `open`'s comment and `todo!` still say "std or tokio". |
| D25 / D26 | `VaultOptions` not present ✅. `tracing` is a dependency, not used yet. |
| D27 | ❌ Add `#[negative_impl] impl !Send for Claim<'_> {}` (and `!Sync` per P18). The scoped-thread test then **won't compile**: spawned closures capture `&mut session1`, which needs `Claim: Send`. Rewrite it so each thread acquires its own claim from a shared `&Vault` (or a clone). |
| D30 | ❌ Add `Vault::new_shared(root, gid: u32)`. |
| Test | ❌ `Vault::new("~/vault")` (std doesn't expand `~`); `try_claim("session_456/config.json")` + `sub_claim("config.json")` resolves to `…/config.json/config.json`; `mod test` lacks `#[cfg(test)]`. |
