//! A driver for work-stealing runtimes, which run tasks on a pool of threads across cores.
//! `tokio` is notably the most widely used. Objects travel as `Arc<dyn Any + Send + Sync>`,
//! and every type is `Send`.
use super::{IntoStorePtr, IoDriver};
use crate::Store;
use crate::io_driver::IntoObjectPtr;
use futures::future::BoxFuture;
use std::any::Any;
use std::sync::Arc;

pub struct AsyncIoDriver;

impl IoDriver for AsyncIoDriver {
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type ObjectPtr = Arc<dyn Any + Send + Sync>;
    type StorePtr = Arc<dyn Store<AsyncIoDriver> + Send + Sync>;
}

impl<S: Store<AsyncIoDriver> + Send + Sync> IntoStorePtr<S> for AsyncIoDriver {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Arc::new(store)
    }
}

impl<T: Any + Send + Sync> IntoObjectPtr<T> for AsyncIoDriver {
    fn from_object_ptr(ptr: Self::ObjectPtr) -> Option<T> {
        ptr.downcast::<T>().ok().and_then(Arc::into_inner)
    }

    fn into_object_ptr(obj: T) -> Self::ObjectPtr {
        Arc::new(obj)
    }
}
