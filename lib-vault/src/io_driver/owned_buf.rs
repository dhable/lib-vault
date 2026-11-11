//! A driver for thread-per-core runtimes, such as completion-based io_uring runtimes. These
//! runtimes normally assign a thread to a specific core, and threads do not pull pending work
//! or reassign work to other threads. Objects travel as `Rc<dyn Any>`, and nothing is `Send`.
use super::{IntoStorePtr, IoDriver};
use crate::Store;
use crate::io_driver::IntoObjectPtr;
use futures::future::LocalBoxFuture;
use std::any::Any;
use std::rc::Rc;

pub struct OwnedBufDriver;

impl IoDriver for OwnedBufDriver {
    type Fut<'a, T: 'a> = LocalBoxFuture<'a, T>;
    type ObjectPtr = Rc<dyn Any>;
    type StorePtr = Rc<dyn Store<OwnedBufDriver>>;
}

impl<S: Store<OwnedBufDriver>> IntoStorePtr<S> for OwnedBufDriver {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Rc::new(store)
    }
}

impl<T: Any> IntoObjectPtr<T> for OwnedBufDriver {
    fn from_object_ptr(ptr: Self::ObjectPtr) -> Option<T> {
        ptr.downcast::<T>().ok().and_then(Rc::into_inner)
    }

    fn into_object_ptr(obj: T) -> Self::ObjectPtr {
        Rc::new(obj)
    }
}
