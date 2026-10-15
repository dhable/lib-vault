use crate::{Error, Key, rt};

// TODO: this is temporary until the sketch it further completed
// use crate::rt::shared::{ObjectReader, ObjectWriter};

/// Interface required for any strategy used for storing the actual object data. Our first
/// implementation will be a file system but it could also be a database, object store or
/// in memory.
///
/// Note for implementors: the returned futures borrow `self` (`'a`) but not `key`. Clone the
/// key (cheap: it's an `Arc<str>`) into the future rather than borrowing it.
pub trait Store<M: rt::IoMode>: 'static {
    /// Returns the keys of all objects beneath `key` (component-wise), relative to `key`.
    fn list(&self, key: &Key) -> M::Fut<'_, Result<Vec<Key>, Error>>;

    /// Deletes the object at `key`. Succeeds if there is no such object.
    fn delete<'a>(&'a self, key: &Key) -> M::Fut<'a, Result<(), Error>>;

    /// Opens the object at `key` for reading. Errors if there is no such object.
    fn open_read<'a>(&'a self, key: &Key) -> M::Fut<'a, Result<M::Reader<'a>, Error>>;

    /// Opens the object at `key` for writing, creating or overwriting it. Implementations
    /// document when written data is committed (e.g. on `close()`). Must fail if `key` differs
    /// only by ASCII case from an existing object's key, so `Foo` and `foo` never coexist.
    fn open_write<'a>(&'a self, key: &Key) -> M::Fut<'a, Result<M::Writer<'a>, Error>>;
}
