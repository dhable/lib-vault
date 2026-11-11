# lib-vault-sqlite

SQLite backend for [`lib-vault`](../lib-vault) that stores the app's own typed values rather
than bytes: a `SqliteStore`, its I/O driver `SqliteDriver`, and a `SqliteClaimer`.

## Model

The app implements `Dispatcher`:

- `type Value`: the one value type the vault stores, usually an enum with a variant per
  struct (`enum AppValue { Doc(MyDataObj), User(User) }`).
- `migrate`, `read`, `write`, `delete`, `list`: how values map onto tables, run on a
  `sqlx::SqliteConnection`. Hand-written queries, SeaORM, or anything else.

The vault type names the dispatcher, `Vault<SqliteDriver<AppDispatcher>>`, and the claim
stores and loads the app's own types:

```rust,ignore
// Any `impl Into<AppValue>`; committed in a transaction.
claim.store(None, MyDataObj { title: "hello".into() }).await?;

// Any `TryFrom<AppValue>`. `Error::NotFound` if missing, `Error::WrongType` if the stored
// value is another variant.
let doc: MyDataObj = claim.load(None).await?;
```

`AppValue` itself works in both calls too. A type with no conversion to the value type is a
compile error, not a run-time one.

The crate docs have a complete, runnable example, and `tests/sqlite/app.rs` a fuller
dispatcher with two record types.

The store passes keys straight through, so the dispatcher upholds the `Store` contract:
full keys, `delete` of a missing key succeeds, `list` is component-wise and relative, and
`write` refuses keys that differ from an existing one only by ASCII case (a unique index with
`COLLATE NOCASE` does it). `store` and `delete` run in a `BEGIN IMMEDIATE` transaction, so a
`store` that fails, or whose future is dropped, is rolled back.

## Claimer

`SqliteClaimer` records claims in `vault_claims` and `vault_claim_owners` tables, so processes
sharing the database file exclude each other. It works with any store, and `SqliteStore`
works with any claimer.

- **Release** is immediate within the claimer (and its clones). Other processes see it once a
  background task deletes the row, normally within milliseconds.
- **Leases**: the background task renews the claimer's lease (`SqliteClaimerOptions::lease`,
  default 30 s). A crashed process's claims free up once its lease runs out. A process whose
  runtime stalls for longer than the lease can lose its claims, so keep the lease well above
  any expected stall.
- **Shutdown**: keep a clone of the claimer and, after `vault.shutdown().await`, call
  `claimer.shutdown().await` before closing the pool, so other processes don't wait for the
  lease.

## Requirements

- Rust 1.94+ (sqlx 0.9).
- A tokio runtime (sqlx with `runtime-tokio`; the claimer spawns a background task).
- SQLite is bundled (sqlx's `sqlite` feature). Use WAL journal mode for concurrent access.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
