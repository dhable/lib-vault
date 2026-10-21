use crate::error::{dispatch_error, store_error};
use crate::{Dispatcher, SqliteDriver, SqliteError, SqliteReader, SqliteWriter};
use futures::future::BoxFuture;
use lib_vault::{Error, Key, Store};
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;

/// A `Store` that keeps typed values in SQLite, using the app's [`Dispatcher`] for every
/// operation. Clones share the pool and the dispatcher.
///
/// Reads run on a pooled connection; writes and deletes run in a `BEGIN IMMEDIATE`
/// transaction, so they take SQLite's write lock up front instead of failing to upgrade
/// a read lock part-way through.
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
        {
            let mut conn = pool.acquire().await?;
            dispatcher
                .migrate(&mut conn)
                .await
                .map_err(SqliteError::Dispatch)?;
        }
        Ok(Self {
            pool,
            dispatcher: Arc::new(dispatcher),
        })
    }

    // Writes `value` at `key` in one transaction: the writer's commit point.
    pub(crate) async fn commit(&self, key: &Key, value: D::Value) -> Result<(), Error> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        self.dispatcher
            .write(&mut tx, key, value)
            .await
            .map_err(dispatch_error)?;
        tx.commit().await.map_err(store_error)
    }
}

// By hand: deriving would require `D: Clone`.
impl<D: Dispatcher> Clone for SqliteStore<D> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            dispatcher: self.dispatcher.clone(),
        }
    }
}

impl<D: Dispatcher> Store<SqliteDriver<D>> for SqliteStore<D> {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        let key = key.clone();
        Box::pin(async move {
            let mut conn = self.pool.acquire().await.map_err(store_error)?;
            self.dispatcher
                .list(&mut conn, &key)
                .await
                .map_err(dispatch_error)
        })
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        let key = key.clone();
        Box::pin(async move {
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(store_error)?;
            self.dispatcher
                .delete(&mut tx, &key)
                .await
                .map_err(dispatch_error)?;
            tx.commit().await.map_err(store_error)
        })
    }

    /// Reads the value now, so a missing key fails here with `Error::NotFound` (D43).
    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<SqliteReader<'a, D>, Error>> {
        let key = key.clone();
        Box::pin(async move {
            let mut conn = self.pool.acquire().await.map_err(store_error)?;
            match self
                .dispatcher
                .read(&mut conn, &key)
                .await
                .map_err(dispatch_error)?
            {
                Some(value) => Ok(SqliteReader::new(key, value)),
                None => Err(Error::NotFound { key }),
            }
        })
    }

    /// Nothing is written until the writer's `close`.
    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<SqliteWriter<'a, D>, Error>> {
        let key = key.clone();
        Box::pin(async move { Ok(SqliteWriter::new(self, key)) })
    }
}
