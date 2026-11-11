use crate::error::{dispatch_error, store_error};
use crate::{Dispatcher, SqliteDriver, SqliteError};
use futures::future::BoxFuture;
use lib_vault::{Error, Key, Store};
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;

/// A `Store` that keeps typed values in SQLite, using the app's [`Dispatcher`] for every
/// operation. Clones share the pool and the dispatcher.
///
/// `load` and `list` run on a pooled connection; `store` and `delete` run in a `BEGIN
/// IMMEDIATE` transaction, so they take SQLite's write lock up front instead of failing to
/// upgrade a read lock part-way through. A `store` is atomic: the dispatcher's `write` is
/// committed whole, or rolled back if it fails or the future is dropped.
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

    // Writes `value` at `key` in one transaction.
    async fn commit(&self, key: &Key, value: D::Value) -> Result<(), Error> {
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

    /// A missing key is `Error::NotFound` (D43).
    fn load<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<D::Value, Error>> {
        let key = key.clone();
        Box::pin(async move {
            let mut conn = self.pool.acquire().await.map_err(store_error)?;
            self.dispatcher
                .read(&mut conn, &key)
                .await
                .map_err(dispatch_error)?
                .ok_or(Error::NotFound { key })
        })
    }

    /// Runs the dispatcher's `write`; the value is stored when this resolves to `Ok`.
    fn store<'a>(&'a self, key: &Key, value: D::Value) -> BoxFuture<'a, Result<(), Error>> {
        let key = key.clone();
        Box::pin(async move { self.commit(&key, value).await })
    }
}
