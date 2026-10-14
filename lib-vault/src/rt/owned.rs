//! Defines a series of I/O types for runtimes that operate on moving owned buffers
//! around. These runtimes normally assign a thread to a specific core and threads
//! do not pull pending work or reassign work to other threads.
use crate::Error;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use std::pin::Pin;

pub type ObjectReader<'a> = BoxStream<'a, Result<Bytes, Error>>;

pub trait ObjectWrite {
    fn write(&mut self, chunk: Bytes) -> BoxFuture<'_, Result<(), Error>>;
    fn close(self: Box<Self>) -> BoxFuture<'static, Result<(), Error>>;
}

pub type ObjectWriter<'a> = Pin<Box<dyn ObjectWrite + 'a>>;

pub struct OwnedMode;
impl super::private::Sealed for OwnedMode {}
impl super::IoMode for OwnedMode {
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type Reader<'a> = ObjectReader<'a>;
    type Writer<'a> = ObjectWriter<'a>;
}
