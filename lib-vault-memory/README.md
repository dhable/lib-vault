# lib-vault-memory

In-memory backend for [`lib-vault`](../lib-vault): a `MemoryStore` and a `MemoryClaimer`.

For tests, examples and prototyping. **Not for production**: nothing is persisted, and
exclusion only covers this process.

## Example

```rust
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::Key;

# futures::executor::block_on(async {
let vault = lib_vault_memory::vault();
let mut claim = vault.claim(Key::new("session")?).await?;

let mut writer = claim.open_write("notes.md").await?;
writer.write_all(b"hello").await?;
writer.close().await?; // commits
drop(writer);

let mut text = String::new();
claim.open_read("notes.md").await?.read_to_string(&mut text).await?;
assert_eq!(text, "hello");
# Ok::<(), Box<dyn std::error::Error>>(())
# }).unwrap();
```

To share data or exclusion between vaults, build them from clones of one store and claimer:

```rust
use lib_vault::Vault;
use lib_vault_memory::{MemoryClaimer, MemoryStore};

let (store, claimer) = (MemoryStore::new(), MemoryClaimer::new());
let a: Vault<lib_vault::io_driver::AsyncIoDriver> = Vault::new(store.clone(), claimer.clone());
let b: Vault<lib_vault::io_driver::AsyncIoDriver> = Vault::new(store, claimer);
```

## Behavior

- **Writes commit on `close()`**, replacing the object atomically. A writer dropped without
  `close()` discards its data.
- **Reads** return a snapshot of the object as of `open_read`.
- **Case collisions**: writing `notes.md` when `Notes.md` exists fails with
  `MemoryError::CaseCollision` (wrapped in `Error::StoreError`).
- **Exclusion** covers one `MemoryClaimer` and its clones. Separate claimers, and other
  processes, don't see each other's claims.
- **Lifetime**: data lives until the last clone of the store is dropped.

## Requirements

- `AsyncIoDriver` only.
- No async runtime: works with tokio, any other runtime, or `futures::executor::block_on`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
