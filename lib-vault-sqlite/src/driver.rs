use crate::{Dispatcher, SqliteReader, SqliteWriter};
use futures::future::BoxFuture;
use lib_vault::Store;
use lib_vault::io_driver::{IntoStorePtr, IoDriver};
use std::marker::PhantomData;
use std::sync::Arc;

/// The I/O driver for a SQLite vault (D77): handles carry the dispatcher's
/// [`Value`](Dispatcher::Value) instead of bytes.
///
/// Futures are `Send` (`BoxFuture`), and the store is held in an `Arc`, so
/// `Vault<SqliteDriver<D>>` and its claims can move between tokio's worker threads.
pub struct SqliteDriver<D>(PhantomData<D>);

impl<D: Dispatcher> IoDriver for SqliteDriver<D> {
    type Reader<'a> = SqliteReader<'a, D>;
    type Writer<'a> = SqliteWriter<'a, D>;
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type StorePtr = Arc<dyn Store<SqliteDriver<D>> + Send + Sync>;
}

impl<D: Dispatcher, S: Store<SqliteDriver<D>> + Send + Sync> IntoStorePtr<S> for SqliteDriver<D> {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Arc::new(store)
    }
}
