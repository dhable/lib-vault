use lib_vault::Key;
use sqlx::sqlite::SqliteConnection;

/// Errors returned by a [`Dispatcher`]. The store wraps them in
/// [`SqliteError::Dispatch`](crate::SqliteError::Dispatch).
pub type DispatchError = Box<dyn std::error::Error + Send + Sync>;

/// App-supplied code that turns the app's values into SQL operations and back.
///
/// A vault holds one value type, [`Dispatcher::Value`], usually an app enum with one variant
/// per struct it stores (`enum AppValue { Doc(MyDataObj), User(User) }`). The dispatcher
/// decides how each variant maps onto tables and columns: hand-written queries, a mapping
/// layer such as SeaORM, or anything else that runs on a [`SqliteConnection`].
///
/// [`SqliteStore`](crate::SqliteStore) passes keys straight through, so a dispatcher upholds
/// the `Store` contract itself:
///
/// - Every key is a **full key** (the claim's key joined with the relative one, D36).
/// - `delete` of a missing key **succeeds** (D43).
/// - `list` returns keys **beneath** the given key, compared component-wise and relative to it,
///   never the key itself (D40).
/// - `write` must **refuse a key that differs only by ASCII case** from an existing one, so
///   `Foo` and `foo` never coexist (D6).
/// - `write` runs **inside a transaction** that the store commits when the writer is closed.
///
/// Every method receives a connection from the store's pool, and its future must be `Send`.
pub trait Dispatcher: Send + Sync + 'static {
    /// The values this vault stores.
    type Value: Send + 'static;

    /// Creates or upgrades the app's tables. Run once by
    /// [`SqliteStore::new`](crate::SqliteStore::new).
    fn migrate(
        &self,
        conn: &mut SqliteConnection,
    ) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Loads the value at `key`, or `None` if there is none.
    fn read(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
    ) -> impl Future<Output = Result<Option<Self::Value>, DispatchError>> + Send;

    /// Stores `value` at `key`, creating or replacing it.
    fn write(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
        value: Self::Value,
    ) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Removes the value at `key`. Succeeds if there is none.
    fn delete(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
    ) -> impl Future<Output = Result<(), DispatchError>> + Send;

    /// Returns the keys of all values beneath `key`, relative to `key`.
    fn list(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
    ) -> impl Future<Output = Result<Vec<Key>, DispatchError>> + Send;
}
