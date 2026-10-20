# lib-vault

Core API for confined, exclusively claimed storage: `Vault`, `Claim`, `Key`, and the `Store`
and `Claimer` traits that backends implement.

Apps use this crate together with a backend
([`lib-vault-memory`](../lib-vault-memory), [`lib-vault-posix`](../lib-vault-posix)).
Backend authors implement its traits. It has no async runtime dependency.

## Example

```rust
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::Key;

# futures::executor::block_on(async {
let vault = lib_vault_memory::vault(); // any backend
let mut claim = vault.claim(Key::new("sessions/123")?).await?;

let mut writer = claim.open_write("notes.md").await?; // sessions/123/notes.md
writer.write_all(b"hello").await?;
writer.close().await?;
drop(writer);

let mut text = String::new();
claim.open_read("notes.md").await?.read_to_string(&mut text).await?;
assert_eq!(claim.list(None).await?, [Key::new("notes.md")?]);

claim.release()?;
vault.shutdown().await; // waits for outstanding claims
# Ok::<(), Box<dyn std::error::Error>>(())
# }).unwrap();
```

## Concepts

| Type | Role |
|---|---|
| `Vault<D>` | Entry point. Pairs a store with a claimer and hands out claims. Cheap to clone. |
| `Claim<D>` | Exclusive hold on a key and everything beneath it. The only way to `list`, `delete`, `open_read`, `open_write`. |
| `Key` | Validated, relative, `/`-separated name. Cheap to clone (`Arc<str>`). |
| `Store<D>` | Backend trait for object data. |
| `Claimer` / `ClaimLock` | Backend trait for exclusion, and the lock it returns. |
| `io_driver::AsyncIoDriver` / `io_driver::OwnedBufDriver` | I/O driver: which future, reader and writer types the API uses. |

## Keys

- Relative and canonical: no leading/trailing `/`, no `//`, no `.` or `..`, not empty.
- Portable names: ASCII letters, digits, `.`, `_`, space, `-`; no Windows device names; the
  `.vault` prefix is reserved.
- Limits: 255 bytes per component, 32 components, 1024 bytes total.
- Stores refuse keys that differ from an existing object only by ASCII case.
- Claim methods take `"a/b"`, `&Key` or `Option<&Key>`; `None` means the claimed key itself.

## Claims

- **Conflicts** are component-wise: holding `a/b` blocks `a/b`, `a/b/c` and `a`, not `a/bc`
  or `a/c`.
- **No waiting**: `vault.claim()` fails right away on a conflict. Retry with your own backoff.
- **Release** with `claim.release()` to see errors; dropping a claim also releases it (errors
  are logged via `tracing`).
- **Handles borrow the claim** mutably: one reader or writer at a time, and a claim can't be
  released while a handle is open.
- **Shutdown**: `vault.shutdown()` refuses new claims and waits for outstanding ones.

## I/O drivers

| | `AsyncIoDriver` | `OwnedBufDriver` |
|---|---|---|
| Runtimes | Work-stealing (tokio, async-std, smol) | Thread-per-core (monoio, tokio-uring, glommio) |
| Futures | `BoxFuture` (`Send`) | `LocalBoxFuture` |
| Reader / writer | `futures` `AsyncRead` / `AsyncWrite` | Stream of `Bytes` / `ObjectWrite` (owned buffers) |
| `Vault` / `Claim` | `Send` (`Vault` is also `Sync`) | `!Send`: one vault per thread |

Both current backends implement `AsyncIoDriver`. `IoDriver` is open: a backend may define its own driver (for example, typed values for a structured store) and implement `IntoStorePtr<S>` for it.

## Writing a backend

```rust,ignore
pub trait Store<D: io_driver::IoDriver>: 'static {
    fn list(&self, key: &Key) -> D::Fut<'_, Result<Vec<Key>, Error>>;
    fn delete<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<(), Error>>;
    fn open_read<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<D::Reader<'a>, Error>>;
    fn open_write<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<D::Writer<'a>, Error>>;
}

pub trait Claimer: Send + Sync + 'static {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>>;
}

pub trait ClaimLock: Send {
    fn release(self: Box<Self>) -> Result<(), Error>;
}
```

Contract checklist:

- [ ] Keys passed in are full keys (already scoped to the claim); never reach outside your root.
- [ ] `list` returns keys strictly beneath `key`, relative to it, matched by whole components.
- [ ] `delete` of a missing object succeeds; `open_read` of one returns `Error::NotFound`.
- [ ] `open_write` creates or overwrites, and refuses a case-only collision. Document when
      data is committed (e.g. on `close()`).
- [ ] Futures borrow `self`, not `key`: clone the key into the future.
- [ ] `try_lock` enforces the conflict rule above across every holder sharing the storage, never
      waits, and releases partial locks if the future is dropped.
- [ ] `release` returns `Ok` only once the key can be claimed again.
- [ ] Wrap backend errors in `Error::StoreError` / `Error::ClaimError`.

[`lib-vault-memory`](../lib-vault-memory) is the reference implementation.

## Errors

| `Error` variant | Meaning |
|---|---|
| `ShuttingDown` | `shutdown()` was called; no new claims |
| `Key(KeyError)` | A string wasn't a valid key |
| `AlreadyClaimed { key }` | An overlapping claim is held |
| `NotFound { key }` | No object at the key |
| `ClaimError(..)` | Backend failure while claiming or releasing |
| `StoreError(..)` | Backend failure in a store operation; downcast for details |

The enum is `#[non_exhaustive]`.

## More

Design and rationale: [`docs/design.md`](../docs/design.md) §2–§7,
[`docs/decisions.md`](../docs/decisions.md).
