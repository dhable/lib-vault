use crate::error::dispatch_error;
use crate::{ClickhouseDriver, ClickhouseError, Dispatcher};
use clickhouse::Client;
use futures::future::BoxFuture;
use lib_vault::{Error, Key, Store};
use std::sync::Arc;

/// A `Store` that keeps typed values in ClickHouse, using the app's [`Dispatcher`] for every
/// operation. Clones share the client and the dispatcher.
///
/// The store hands the dispatcher a clone of the client with **asynchronous inserts turned
/// off** (`async_insert = 0`), so `store` returns only once the server has written the data
/// and a following `load` sees it. ClickHouse has no transactions: a `store` that fails
/// part-way can leave part of the dispatcher's `write` applied.
///
/// Works with any claimer: [`ClickhouseClaimer`](crate::ClickhouseClaimer) for processes and
/// servers sharing a Keeper, or e.g. `MemoryClaimer` within one process.
pub struct ClickhouseStore<D: Dispatcher> {
    client: Client,
    dispatcher: Arc<D>,
}

impl<D: Dispatcher> ClickhouseStore<D> {
    /// Creates a store over `client` and runs the dispatcher's
    /// [`migrate`](Dispatcher::migrate).
    pub async fn new(client: Client, dispatcher: D) -> Result<Self, ClickhouseError> {
        let client = client.with_setting("async_insert", "0");
        dispatcher
            .migrate(&client)
            .await
            .map_err(ClickhouseError::Dispatch)?;
        Ok(Self {
            client,
            dispatcher: Arc::new(dispatcher),
        })
    }
}

// By hand: deriving would require `D: Clone`.
impl<D: Dispatcher> Clone for ClickhouseStore<D> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            dispatcher: self.dispatcher.clone(),
        }
    }
}

impl<D: Dispatcher> Store<ClickhouseDriver<D>> for ClickhouseStore<D> {
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        let key = key.clone();
        Box::pin(async move {
            self.dispatcher
                .list(&self.client, &key)
                .await
                .map_err(dispatch_error)
        })
    }

    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        let key = key.clone();
        Box::pin(async move {
            self.dispatcher
                .delete(&self.client, &key)
                .await
                .map_err(dispatch_error)
        })
    }

    /// A missing key is `Error::NotFound` (D43).
    fn load<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<D::Value, Error>> {
        let key = key.clone();
        Box::pin(async move {
            self.dispatcher
                .read(&self.client, &key)
                .await
                .map_err(dispatch_error)?
                .ok_or(Error::NotFound { key })
        })
    }

    /// Runs the dispatcher's `write`; the value is stored when this resolves to `Ok`.
    fn store<'a>(&'a self, key: &Key, value: D::Value) -> BoxFuture<'a, Result<(), Error>> {
        let key = key.clone();
        Box::pin(async move {
            self.dispatcher
                .write(&self.client, &key, value)
                .await
                .map_err(dispatch_error)
        })
    }
}
