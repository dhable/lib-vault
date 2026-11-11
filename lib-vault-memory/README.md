# lib-vault-memory

In-memory backend for [`lib-vault`](../lib-vault): a `MemoryStore` and a `MemoryClaimer`.

For tests, examples and prototyping. **Not for production**: nothing is persisted, and
exclusion only covers this process.

## Example

```rust
use lib_vault::Key;

# futures::executor::block_on(async {
let vault = lib_vault_memory::vault();
let mut claim = vault.claim(Key::new("session")?).await?;

claim.store("notes.md", b"hello".to_vec()).await?;

let bytes: Vec<u8> = claim.load("notes.md").await?;
assert_eq!(bytes, b"hello");
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

- **Objects are `Vec<u8>`**: `claim.store(key, bytes)` and `claim.load::<Vec<u8>>(key)`. Any
  other type fails with `Error::WrongType`.
- **`store`** replaces the object atomically: it stores all of the bytes, or fails and leaves
  the key as it was.
- **`load`** returns a copy of the object's bytes.
- **Case collisions**: storing `notes.md` when `Notes.md` exists fails with
  `MemoryError::CaseCollision` (wrapped in `Error::StoreError`).
- **Exclusion** covers one `MemoryClaimer` and its clones. Separate claimers, and other
  processes, don't see each other's claims.
- **Lifetime**: data lives until the last clone of the store is dropped.

## Requirements

- `AsyncIoDriver` only.
- No async runtime: works with tokio, any other runtime, or `futures::executor::block_on`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
