# lib-vault-posix

Filesystem backend for [`lib-vault`](../lib-vault): `FsStore` keeps objects as files under a
vault root, and `FsClaimer` locks them with `flock` across threads and processes.

> **Requirements**
> - **tokio runtime**: operations run on tokio's blocking pool and panic outside a runtime.
> - **Linux ≥ 5.6 or macOS ≥ 15**, **Rust ≥ 1.89**.
> - **Local filesystem**: network filesystems are allowed but logged as a warning, since
>   `flock` is unreliable across hosts.

## Example

```rust,no_run
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::Key;
use lib_vault_posix::{FsStoreOptions, Sharing};

# #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
let options = FsStoreOptions::default(); // or .sharing(Sharing::Group(gid))
let vault = lib_vault_posix::vault("/srv/app/vault", options)?;
let mut claim = vault.claim(Key::new("session")?).await?;

let mut writer = claim.open_write("notes.md").await?;
writer.write_all(b"hello").await?;
writer.close().await?; // the commit point
drop(writer);

let mut text = String::new();
claim.open_read("notes.md").await?.read_to_string(&mut text).await?;
claim.release()?;
# Ok(()) }
```

`vault()` is shorthand for `FsStore::new(root, options)` plus `store.claimer()`. Always pair a
store with its own claimer.

## Options

| `FsStoreOptions` | Default | Effect |
|---|---|---|
| `sharing(Sharing)` | `Private` | `Private`: root `0700`, files `0600`, owned by the euid. `Group(gid)`: root `2770` (setgid), files `0660`, for apps running as different uids. |
| `journaling(bool)` | `true` | Write through a journal so a crash never leaves a partial object. Costs ~2× bytes written. |
| `strict_modes(bool)` | `true` | sshd-style check that no ancestor of the root is writable by others. |
| `max_verify_retries(u32)` | `3` | Retries when a lock file changes while being locked; `0` fails on the first change. |

## On disk

```text
/srv/app/vault/                       root (0700 or 2770)
├── .vault-lock-sessions              lock file for the `sessions` component
└── sessions/
    ├── .vault-lock-123               lock file for `sessions/123`, removed on release
    └── 123/
        ├── notes.md                  object: a plain file at its key path
        └── .vault-journal-notes.md   only during a write, or after a crash
```

- Lock and journal files use the reserved `.vault` prefix: never valid keys, never listed.
- A claim takes shared `flock`s on its ancestors' lock files and an exclusive one on its own.
- Objects are created on the first write; claiming a missing key creates no object.
- Names too long for a lock or journal file name are hashed (`.vault-lock-#<hash>`).

## Safety

- **Refused**: symlinks anywhere in a path, hard-linked files, FIFOs/sockets/devices, mount
  crossings. Every open is handle-relative with `O_NOFOLLOW`.
- **Checked when the store is created**: the root is a real directory, not `/` or a system
  directory; it matches the sharing mode; it has no extended ACLs; and, with
  `strict_modes`, its ancestors are safe.
- **Not allowed**: nested vault roots (one vault inside another). Not detected, so exclusion
  between them isn't guaranteed.

## Recovering an interrupted write

If a process dies mid-commit, the journal stays and `open_read`/`open_write` on that key fail
with `PosixError::InterruptedWrite { key, journal }`. Other keys are unaffected. Fix by hand:

- **Complete journal** (header starts with `LVJRNL01`; its length and CRC-32
  match the content): the new content follows the 32-byte header. Copy it over the object (`tail -c +33 journal > object`), then delete the journal.
- **Incomplete journal** (all-zero header): delete it; the object was never touched.

## Errors

Construction returns `PosixError`. Claim and store operations wrap it in
`lib_vault::Error::ClaimError` / `StoreError`; downcast to inspect it.

| `PosixError` | Meaning |
|---|---|
| `InvalidRoot` | Root is missing, a symlink, a system directory, or has the wrong owner or mode |
| `StrictModes` | An ancestor of the root is unsafe |
| `ExtendedAcl` | Root has extended ACLs |
| `InterruptedWrite` | Leftover journal; see above |
| `CaseCollision` | Key differs only by case from an existing object |
| `Unsupported` | Symlink, special file, hard link or mount point in the way |
| `VerifyRetriesExhausted` | A lock file kept changing while being locked |
| `Io` | I/O error, with the path |

## Deployment tips

- Run harness instances as dedicated uids that share a vault group (`Sharing::Group`).
- Linux: mount the vault filesystem `nosymfollow,nodev,nosuid,noexec`. macOS: a dedicated APFS
  volume mounted `nodev,nosuid,noexec`.
- Cap disk use with a size-limited filesystem, quotas or `RLIMIT_FSIZE`.

## More

Protocol and security model: [`docs/design.md`](../docs/design.md) §8–§9.

Tested on macOS; Linux testing is pending.
