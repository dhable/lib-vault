use crate::error::store_error;
use crate::{ClickhouseError, ClickhouseStore, Dispatcher};
use futures::future::BoxFuture;
use lib_vault::{Error, Key};
use std::marker::PhantomData;

/// The value at a key, fetched by `open_read`. Borrows the claim like any other handle (D42).
pub struct ClickhouseReader<'a, D: Dispatcher> {
    key: Key,
    value: D::Value,
    _claim: PhantomData<&'a mut ()>,
}

impl<D: Dispatcher> ClickhouseReader<'_, D> {
    pub(crate) fn new(key: Key, value: D::Value) -> Self {
        Self {
            key,
            value,
            _claim: PhantomData,
        }
    }

    /// The full key the value was read from.
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The value, as the dispatcher's value type.
    pub fn value(&self) -> &D::Value {
        &self.value
    }

    /// Takes the value, as the dispatcher's value type.
    pub fn into_value(self) -> D::Value {
        self.value
    }

    /// Takes the value as one of the app's types, e.g. `reader.try_as::<MyDataObj>()`.
    /// Fails with `Error::StoreError` wrapping [`ClickhouseError::WrongType`] if the
    /// conversion from the value type fails.
    pub fn try_as<T: TryFrom<D::Value>>(self) -> Result<T, Error> {
        let Self { key, value, .. } = self;
        T::try_from(value).map_err(|_| store_error(ClickhouseError::WrongType { key }))
    }
}

/// Writes one value to a key. [`put`](Self::put) buffers the value, and
/// [`close`](Self::close) is the commit point (D43): it runs the dispatcher's `write`. A
/// writer dropped without `close` discards its value.
pub struct ClickhouseWriter<'a, D: Dispatcher> {
    store: &'a ClickhouseStore<D>,
    key: Key,
    pending: Option<D::Value>,
}

impl<'a, D: Dispatcher> ClickhouseWriter<'a, D> {
    pub(crate) fn new(store: &'a ClickhouseStore<D>, key: Key) -> Self {
        Self {
            store,
            key,
            pending: None,
        }
    }

    /// Sets the value to write, replacing any earlier `put` on this writer.
    pub fn put(&mut self, value: impl Into<D::Value>) {
        self.pending = Some(value.into());
    }

    /// Commits the value. Closing without a `put` leaves the key unchanged.
    ///
    /// Returns a boxed `Send` future rather than being an `async fn`: the future borrows the
    /// store for `'a`, and inside a spawned task the compiler can't otherwise prove such a
    /// future `Send` for every `'a`.
    pub fn close(self) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            match self.pending {
                Some(value) => self.store.commit(&self.key, value).await,
                None => Ok(()),
            }
        })
    }
}
