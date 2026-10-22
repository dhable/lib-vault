use crate::{ClickhouseReader, ClickhouseWriter, Dispatcher};
use futures::future::BoxFuture;
use lib_vault::Store;
use lib_vault::io_driver::{IntoStorePtr, IoDriver};
use std::marker::PhantomData;
use std::sync::Arc;

/// The I/O driver for a ClickHouse vault (D77): handles carry the dispatcher's
/// [`Value`](Dispatcher::Value) instead of bytes.
///
/// Futures are `Send` (`BoxFuture`), and the store is held in an `Arc`, so
/// `Vault<ClickhouseDriver<D>>` and its claims can move between tokio's worker threads.
pub struct ClickhouseDriver<D>(PhantomData<D>);

impl<D: Dispatcher> IoDriver for ClickhouseDriver<D> {
    type Reader<'a> = ClickhouseReader<'a, D>;
    type Writer<'a> = ClickhouseWriter<'a, D>;
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type StorePtr = Arc<dyn Store<ClickhouseDriver<D>> + Send + Sync>;
}

impl<D: Dispatcher, S: Store<ClickhouseDriver<D>> + Send + Sync> IntoStorePtr<S>
    for ClickhouseDriver<D>
{
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Arc::new(store)
    }
}
