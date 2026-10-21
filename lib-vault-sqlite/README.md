# lib-vault-sqlite

SQLite backend for [`lib-vault`](../lib-vault) that stores the app's own typed values rather
than bytes: a `SqliteStore`, its I/O driver `SqliteDriver`, and a `SqliteClaimer`.

**Status: API sketch.** Every method is `todo!()`; nothing talks to a database yet.

## Model

The app implements `Dispatcher`:

- `type Value`: the one value type the vault stores, usually an enum with a variant per
  struct (`enum AppValue { Doc(MyDataObj), User(User) }`).
- `migrate`, `read`, `write`, `delete`, `list`: how values map onto tables, run on a
  `sqlx::SqliteConnection`. Hand-written queries, SeaORM, or anything else.

The vault type names the dispatcher, `Vault<SqliteDriver<AppDispatcher>>`, and the claim
hands out typed handles:

```rust,ignore
let mut writer = claim.open_write(None).await?;
writer.put(MyDataObj { title: "hello".into() }); // impl Into<AppValue>
writer.close().await?;                           // commits in a transaction

let reader = claim.open_read(None).await?;       // Error::NotFound if missing
let doc: MyDataObj = reader.try_as()?;           // TryFrom<AppValue>
```

The store passes keys straight through, so the dispatcher upholds the `Store` contract:
full keys, `delete` of a missing key succeeds, `list` is component-wise and relative, and
`write` refuses keys that differ from an existing one only by ASCII case.

## Claimer

`SqliteClaimer` records claims in a `vault_claims` table, so processes sharing the database
file exclude each other. It works with any store, and `SqliteStore` works with any claimer.
Two problems are still open:

- `ClaimLock::release` must be synchronous, with `Ok` meaning the key is free immediately,
  but deleting the row is async work.
- A crashed process leaves its row behind, so claims need a lease or stale-row recovery.

## Requirements

- A tokio runtime (sqlx with `runtime-tokio`).
