//! Defines a series of I/O types for work stealing runtimes that can run tasks
//! on a pool of threads scheduled on different cores. `tokio` is notability the
//! most widely used shared runtime.
use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncWrite};
use std::pin::Pin;

/// Type definition that must be satisified for the object data reader
/// used by the Store implementation.
pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;

/// Type definition that must be satisified for the object data writer
/// used by the Store implementation.
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

pub struct SharedMode;
impl super::private::Sealed for SharedMode {}
impl super::IoMode for SharedMode {
    type Fut<'a, T: 'a> = BoxFuture<'a, T>;
    type Reader<'a> = ObjectReader<'a>;
    type Writer<'a> = ObjectWriter<'a>;
}
