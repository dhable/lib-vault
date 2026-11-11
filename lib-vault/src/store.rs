use crate::{Error, Key, io_driver};

/// Interface required for any strategy used for storing the actual object data. Our first
/// implementation will be a file system but it could also be a database, object store or
/// in memory.
///
/// Note for implementors: the returned futures borrow `self` (`'a`) but not `key`. Clone the
/// key (cheap: it's an `Arc<str>`) into the future rather than borrowing it.
pub trait Store<D: io_driver::IoDriver>: 'static {
    /// Returns the keys of all objects beneath `key` (component-wise), relative to `key`.
    fn list(&self, key: &Key) -> D::Fut<'_, Result<Vec<Key>, Error>>;

    /// Deletes the object at `key`. Succeeds if there is no such object.
    fn delete<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<(), Error>>;

    /// Loads the whole object at `key`. Errors with `Error::NotFound` if there is no such
    /// object. A shared pointer (`Arc`, `Rc`) must be uniquely owned: the caller takes the
    /// object out of it, so don't hand out a clone of a pointer the store keeps.
    fn load<'a>(&'a self, key: &Key) -> D::Fut<'a, Result<D::ObjectPtr, Error>>;

    /// Stores `data` as the whole object at `key`, creating or overwriting it. Implementations
    /// document whether a failed or interrupted call can leave a partial object. Must fail with
    /// `Error::WrongType` if `data` isn't a type the store holds, and must fail if `key` differs
    /// only by ASCII case from an existing object's key, so `Foo` and `foo` never coexist.
    fn store<'a>(&'a self, key: &Key, data: D::ObjectPtr) -> D::Fut<'a, Result<(), Error>>;
}
