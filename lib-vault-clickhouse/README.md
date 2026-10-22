# lib-vault-clickhouse

ClickHouse backend for [`lib-vault`](../lib-vault) that stores the app's own typed values
rather than bytes: a `ClickhouseStore`, its I/O driver `ClickhouseDriver`, and a
`ClickhouseClaimer` that keeps claims in Keeper through `KeeperMap` tables.

## Model

Like [`lib-vault-sqlite`](../lib-vault-sqlite), the app implements `Dispatcher`:

- `type Value`: the one value type the vault stores, usually an enum with a variant per
  struct (`enum AppValue { Doc(MyDataObj), User(User) }`).
- `migrate`, `read`, `write`, `delete`, `list`: how values map onto tables, using a
  `clickhouse::Client`. Any schema works; `ReplacingMergeTree(version, is_deleted)` tables
  make writes and deletes plain inserts.

```rust,ignore
let vault: Vault<ClickhouseDriver<AppDispatcher>> = Vault::new(
    ClickhouseStore::new(client.clone(), AppDispatcher).await?,
    ClickhouseClaimer::new(client).await?,
);
let mut claim = vault.claim(Key::new("docs/a")?).await?;

let mut writer = claim.open_write(None).await?;
writer.put(MyDataObj { title: "hello".into() }); // impl Into<AppValue>
writer.close().await?;                           // runs the dispatcher's write

let doc: MyDataObj = claim.open_read(None).await?.try_as()?;
```

The store passes keys straight through, so the dispatcher upholds the `Store` contract: full
keys, reads see completed writes, `delete` of a missing key succeeds, `list` is
component-wise and relative, and `write` refuses keys that differ from an existing one only
by ASCII case. ClickHouse has no unique constraints or transactions: a `KeeperMap` index
keyed by the lowercased key, written in strict mode, enforces the case rule, and each write
should be safe to repeat. `tests/clickhouse/app.rs` is a complete example.

## Claimer

`ClickhouseClaimer` creates two `KeeperMap` tables in the client's database:
`vault_claims` (a row per locked key component, with an exclusive holder and shared holders)
and `vault_claim_owners` (a row per live claimer and its lease). Claimers using the same
Keeper path (`ClickhouseClaimerOptions::root_path`, default `/lib_vault`) on servers sharing a
Keeper exclude each other. It works with any store.

- **Claiming** `a/b/c` takes shared holds on `a` and `a/b` and an exclusive hold on `a/b/c`,
  each through a version-checked Keeper write (`keeper_map_strict_mode`), so a stale read can
  refuse a claim but never wrongly grant one.
- **Release** is immediate within the claimer and its clones; other claimers see it once a
  background task updates the rows.
- **Leases**: the background task renews the claimer's lease (`lease`, default 30 s, server
  clock). A crashed process's claims free up once its lease runs out. A process whose runtime
  stalls for longer than the lease can lose its claims.
- **Shutdown**: keep a clone of the claimer and, after `vault.shutdown().await`, call
  `claimer.shutdown().await`.

## Server requirements

Keeper (embedded `clickhouse-keeper` or ZooKeeper) configured with `<zookeeper>`, and
`<keeper_map_path_prefix>` set, as in [`tests/docker/keeper.xml`](tests/docker/keeper.xml).
`KeeperMap` needs an `Atomic` or `Replicated` database (the default).

## Tests

- `cargo test -p lib-vault-clickhouse` runs the unit tests, which need no server.
- The integration tests run against a live server with Keeper:

  ```sh
  docker compose -f lib-vault-clickhouse/tests/docker/compose.yaml up -d --wait
  cargo test -p lib-vault-clickhouse --features integration-tests
  ```

  `LIB_VAULT_CLICKHOUSE_URL` (default `http://localhost:8123`) and
  `LIB_VAULT_CLICKHOUSE_PASSWORD` (default `vault`) select another server. Each test uses its
  own database and Keeper path.

## Requirements

- Rust 1.89+.
- A tokio runtime (the `clickhouse` client; the claimer spawns a background task).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
