use crate::{Dispatcher, SqliteStore};
use lib_vault::{Error, Key};
use std::marker::PhantomData;

/// The value at a key, fetched by `open_read`. Borrows the claim like any other handle (D42).
pub struct SqliteReader<'a, D: Dispatcher> {
    key: Key,
    value: D::Value,
    _claim: PhantomData<&'a mut ()>,
}

impl<D: Dispatcher> SqliteReader<'_, D> {
    /// The full key the value was read from.
    pub fn key(&self) -> &Key {
        todo!()
    }

    /// The value, as the dispatcher's value type.
    pub fn value(&self) -> &D::Value {
        todo!()
    }

    /// Takes the value, as the dispatcher's value type.
    pub fn into_value(self) -> D::Value {
        todo!()
    }

    /// Takes the value as one of the app's types, e.g. `reader.try_as::<MyDataObj>()`.
    /// Fails with `Error::StoreError` wrapping [`SqliteError::WrongType`](crate::SqliteError::WrongType)
    /// if the conversion from the value type fails.
    pub fn try_as<T: TryFrom<D::Value>>(self) -> Result<T, Error> {
        todo!()
    }
}

/// Writes one value to a key. [`put`](Self::put) buffers the value, and
/// [`close`](Self::close) is the commit point (D43): it runs the dispatcher's `write` in a
/// transaction. A writer dropped without `close` discards its value.
pub struct SqliteWriter<'a, D: Dispatcher> {
    store: &'a SqliteStore<D>,
    key: Key,
    pending: Option<D::Value>,
}

impl<D: Dispatcher> SqliteWriter<'_, D> {
    /// Sets the value to write, replacing any earlier `put` on this writer.
    pub fn put(&mut self, value: impl Into<D::Value>) {
        todo!()
    }

    /// Commits the value. Closing without a `put` leaves the key unchanged.
    pub async fn close(self) -> Result<(), Error> {
        todo!()
    }
}
