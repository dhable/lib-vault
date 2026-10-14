//! Defines a series of I/O types for runtimes that operate on moving owned buffers
//! around. These runtimes normally assign a thread to a specific core and threads
//! do not pull pending work or reassign work to other threads.
use crate::Error;
use bytes::Bytes;
use futures::future::LocalBoxFuture;
use futures::stream::LocalBoxStream;

pub type ObjectReader<'a> = LocalBoxStream<'a, Result<Bytes, Error>>;

pub trait ObjectWrite {
    fn write(&mut self, chunk: Bytes) -> LocalBoxFuture<'_, Result<(), Error>>;
    fn close(self: Box<Self>) -> LocalBoxFuture<'static, Result<(), Error>>;
}

pub type ObjectWriter<'a> = Box<dyn ObjectWrite + 'a>;

pub struct OwnedMode;
impl super::private::Sealed for OwnedMode {}
impl super::IoMode for OwnedMode {
    type Fut<'a, T: 'a> = LocalBoxFuture<'a, T>;
    type Reader<'a> = ObjectReader<'a>;
    type Writer<'a> = ObjectWriter<'a>;
}
