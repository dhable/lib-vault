use crate::{Dispatcher, SqliteDriver, SqliteError, SqliteReader, SqliteWriter};
use futures::future::BoxFuture;
use lib_vault::{Error, Key, Store};
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;

/// A `Store` that keeps typed values in SQLite, using the app's [`Dispatcher`] for every
/// operation. Clones share the pool and the dispatcher.
///
/// Works with any claimer: [`SqliteClaimer`](crate::SqliteClaimer) for processes sharing the
/// database file, or e.g. `MemoryClaimer` within one process.
pub struct SqliteStore<D: Dispatcher> {
    pool: SqlitePool,
    dispatcher: Arc<D>,
}

impl<D: Dispatcher> SqliteStore<D> {
    /// Creates a store over `pool` and runs the dispatcher's
    /// [`migrate`](Dispatcher::migrate).
    pub async fn new(pool: SqlitePool, dispatcher: D) -> Result<Self, SqliteError> {
        todo!()
    }
}

// By hand: deriving would require `D: Clone`.
impl<D: Dispatcher> Clone for SqliteStore<D> {
    fn clone(&self) -> Self {
        todo!()
    }
}

impl<D: Dispatcher> Store<SqliteDriver<D>> for SqliteStore<D> {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        todo!()
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        todo!()
    }

    /// Reads the value now, so a missing key fails here with `Error::NotFound` (D43).
    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<SqliteReader<'a, D>, Error>> {
        todo!()
    }

    /// Nothing is written until the writer's `close`.
    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<SqliteWriter<'a, D>, Error>> {
        todo!()
    }
}
