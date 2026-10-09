use crate::{Error, Key, KeyError};
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

/// Interface required for any strategy used for storing the actual object data. Our first
/// implementation will be a file system but it could also be a database, object store or
/// in memory.
///
/// Note for implementors: the returned futures borrow `self` (`'a`) but not `key`. Clone the
/// key (cheap: it's an `Arc<str>`) into the future rather than borrowing it.
pub trait Store: Send + Sync + 'static {
    /// Returns the keys of all objects beneath `key` (component-wise), relative to `key`.
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>>;

    /// Deletes the object at `key`. Succeeds if there is no such object.
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>>;

    /// Opens the object at `key` for reading. Errors if there is no such object.
    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>>;

    /// Opens the object at `key` for writing, creating or overwriting it. Implementations
    /// document when written data is committed (e.g. on `close()`). Must fail if `key` differs
    /// only by ASCII case from an existing object's key, so `Foo` and `foo` never coexist.
    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>>;
}

/// Wraps a Store and prepends a defined key automatically
pub struct ScopedStore {
    base: Key,
    pub(crate) inner: Arc<dyn Store>,
}

impl ScopedStore {
    /// Wraps `inner` so every key is resolved beneath `base`. Since `Key::join` can only
    /// nest, callers of the scoped store can't reach anything outside `base`.
    pub fn new(base: Key, inner: Arc<dyn Store>) -> Self {
        Self { base, inner }
    }

    // The full key for an optional key relative to `base`; `None` is `base` itself.
    pub(crate) fn resolve(&self, key: Option<&Key>) -> Result<Key, KeyError> {
        key.map_or_else(|| Ok(self.base.clone()), |k| self.base.join(k))
    }
}

impl Store for ScopedStore {
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        match self.base.join(key) {
            Ok(key) => self.inner.delete(&key),
            Err(e) => Box::pin(futures::future::ready(Err(e.into()))),
        }
    }

    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        match self.base.join(key) {
            Ok(key) => self.inner.list(&key),
            Err(e) => Box::pin(futures::future::ready(Err(e.into()))),
        }
    }

    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
        match self.base.join(key) {
            Ok(key) => self.inner.open_read(&key),
            Err(e) => Box::pin(futures::future::ready(Err(e.into()))),
        }
    }

    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
        match self.base.join(key) {
            Ok(key) => self.inner.open_write(&key),
            Err(e) => Box::pin(futures::future::ready(Err(e.into()))),
        }
    }
}
