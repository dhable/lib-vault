# lib-vault

Confined, exclusively claimed storage for AI harnesses: memory context, scratchpads, artifacts.

An app asks a `Vault` for a `Claim` on a key such as `sessions/123`. The claim is the only way
to read, write, list or delete objects at that key and beneath it, and no one else (another
task, thread or process) can claim an overlapping key until it's released. Storage and locking
are pluggable backends.

- **Confinement**: keys can't express `..`, absolute paths or links, so a claim can't reach
  outside its subtree.
- **Exclusive claims**: a claim on `a/b` blocks `a/b`, `a/b/c` and `a`, but not `a/c`.
- **Pluggable backends**: implement `Store` (data) and `Claimer` (locking).
- **Runtime-agnostic core**: async API, no runtime dependency; each backend states its own.
- **Crash-safe filesystem backend**: journaled writes, `flock` exclusion across processes,
  hardened against symlink and permission attacks.

## Crates

| Crate | What it is | Use it when |
|---|---|---|
| [`lib-vault`](lib-vault) | Core API: `Vault`, `Claim`, `Key`, the `Store` and `Claimer` traits | Writing a backend, or naming the types in your app |
| [`lib-vault-memory`](lib-vault-memory) | In-memory backend, no dependencies | Tests, examples, prototyping |
| [`lib-vault-posix`](lib-vault-posix) | Filesystem backend for Linux and macOS (tokio) | Real persistent storage |

## Quick start

```toml
[dependencies]
lib-vault = { path = "lib-vault" }
lib-vault-memory = { path = "lib-vault-memory" }
```

```rust
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::Key;

let vault = lib_vault_memory::vault();
let mut claim = vault.claim(Key::new("session")?).await?;

let mut writer = claim.open_write("notes.md").await?;
writer.write_all(b"hello").await?;
writer.close().await?; // commit
drop(writer);

let mut text = String::new();
claim.open_read("notes.md").await?.read_to_string(&mut text).await?;
claim.release()?;
```

For persistent storage, swap in `lib_vault_posix::vault("/srv/app/vault", FsStoreOptions::default())?`
inside a tokio runtime. See each crate's README for details.

## Choosing a backend

| Backend | Persistent | Exclusion scope | Runtime | Platforms |
|---|---|---|---|---|
| `lib-vault-memory` | No | One claimer (and its clones) | Any, or none | Any |
| `lib-vault-posix` | Yes | All processes using the same root | tokio | Linux ≥ 5.6, macOS ≥ 15 |

## Documentation

- [`docs/design.md`](docs/design.md): threat model, contracts, filesystem protocol, security.
- [`docs/decisions.md`](docs/decisions.md): decision log (D-numbers) and open questions.
- `cargo doc --workspace --no-deps --open`: API reference.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

- Rust 1.89+ (edition 2024; `lib-vault-posix` needs 1.89 for `File::lock`).
- The `lib-vault-posix` tests need Linux or macOS and a local temp directory.

## Status

Pre-1.0 and not published; the API may still change. `lib-vault-posix` is tested on macOS;
Linux testing is pending. Thread-per-core (`OwnedMode`) support is in the core API, but no
backend implements it yet.
