//! A byte-chunk driver for runtimes that move owned buffers (`Bytes`) around, such as
//! completion-based io_uring runtimes. These runtimes normally assign a thread to a specific core and threads
//! do not pull pending work or reassign work to other threads.
use super::{IntoStorePtr, IoDriver};
use crate::{Error, Store};
use bytes::Bytes;
use futures::future::LocalBoxFuture;
use futures::stream::LocalBoxStream;
use std::rc::Rc;

pub type ObjectReader<'a> = LocalBoxStream<'a, Result<Bytes, Error>>;

pub trait ObjectWrite {
    fn write(&mut self, chunk: Bytes) -> LocalBoxFuture<'_, Result<(), Error>>;
    fn close(self: Box<Self>) -> LocalBoxFuture<'static, Result<(), Error>>;
}

pub type ObjectWriter<'a> = Box<dyn ObjectWrite + 'a>;

pub struct OwnedBufDriver;

impl IoDriver for OwnedBufDriver {
    type Fut<'a, T: 'a> = LocalBoxFuture<'a, T>;
    type Reader<'a> = ObjectReader<'a>;
    type Writer<'a> = ObjectWriter<'a>;
    type StorePtr = Rc<dyn Store<OwnedBufDriver>>;
}

impl<S: Store<OwnedBufDriver>> IntoStorePtr<S> for OwnedBufDriver {
    fn into_store_ptr(store: S) -> Self::StorePtr {
        Rc::new(store)
    }
}
