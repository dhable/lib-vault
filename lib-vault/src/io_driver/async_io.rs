//! A byte-stream driver for work-stealing runtimes, which run tasks on a pool of threads
//! across cores. `tokio` is notably the most widely used. Objects are read and written
//! through `futures` `AsyncRead` / `AsyncWrite`, and every type is `Send`.
use super::{IntoStorePtr, IoDriver};
use crate::Store;
use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncWrite};
use std::pin::Pin;
use std::sync::Arc;

/// Type definition that must be satisified for the object data reader
/// used by the Store implementation.
pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;

/// Type definition that must be satisified for the object data writer
/// used by the Store implementation.
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

pub struct AsyncIoDriver;

impl IoDriver for AsyncIoDriver {
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type Reader<'a> = ObjectReader<'a>;
    type Writer<'a> = ObjectWriter<'a>;
    type StorePtr = Arc<dyn Store<AsyncIoDriver> + Send + Sync>;
}

impl<S: Store<AsyncIoDriver> + Send + Sync> IntoStorePtr<S> for AsyncIoDriver {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Arc::new(store)
    }
}
