use clickhouse::Client;
use lib_vault::Key;

/// Errors returned by a [`Dispatcher`]. The store wraps them in
/// [`ClickhouseError::Dispatch`](crate::ClickhouseError::Dispatch).
pub type DispatchError = Box<dyn std::error::Error + Send + Sync>;

/// App-supplied code that turns the app's values into ClickHouse operations and back.
///
/// A vault holds one value type, [`Dispatcher::Value`], usually an app enum with one variant
/// per struct it stores (`enum AppValue { Doc(MyDataObj), User(User) }`). The dispatcher
/// decides how each variant maps onto tables: `MergeTree`-family tables with typed columns
/// (for example `ReplacingMergeTree(version, is_deleted)` so writes and deletes are plain
/// inserts), `KeeperMap` tables, or anything else the [`Client`] can reach.
///
/// [`ClickhouseStore`](crate::ClickhouseStore) passes keys straight through, so a dispatcher
/// upholds the `Store` contract itself:
///
/// - Every key is a **full key** (the claim's key joined with the relative one, D36).
/// - `read` must see a value as soon as the `write` that stored it has returned.
/// - `delete` of a missing key **succeeds** (D43).
/// - `list` returns keys **beneath** the given key, compared component-wise and relative to it,
///   never the key itself (D40).
/// - `write` must **refuse a key that differs only by ASCII case** from an existing one, so
///   `Foo` and `foo` never coexist (D6). ClickHouse has no unique constraints; a `KeeperMap`
///   index keyed by the lowercased key, written with `keeper_map_strict_mode`, is one way.
///
/// ClickHouse has no transactions: a `write` that fails part-way can leave part of it
/// applied, so keep each write to statements that are safe to repeat.
///
/// Every method receives the store's client, and its future must be `Send`.
pub trait Dispatcher: Send + Sync + 'static {
    /// The values this vault stores.
    type Value: Send + 'static;

    /// Creates or upgrades the app's tables. Run once by
    /// [`ClickhouseStore::new`](crate::ClickhouseStore::new).
    fn migrate(&self, client: &Client) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Loads the value at `key`, or `None` if there is none.
    fn read(
        &self,
        client: &Client,
        key: &Key,
    ) -> impl Future<Output = Result<Option<Self::Value>, DispatchError>> + Send;

    /// Stores `value` at `key`, creating or replacing it.
    fn write(
        &self,
        client: &Client,
        key: &Key,
        value: Self::Value,
    ) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Removes the value at `key`. Succeeds if there is none.
    fn delete(
        &self,
        client: &Client,
        key: &Key,
    ) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Returns the keys of all values beneath `key`, relative to `key`.
    fn list(
        &self,
        client: &Client,
        key: &Key,
    ) -> impl Future<Output = Result<Vec<Key>, DispatchError>> + Send;
}
