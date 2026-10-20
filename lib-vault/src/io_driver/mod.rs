//! I/O drivers: the handle, future and store-pointer types that `Vault`, `Claim` and
//! `Store` are generic over.
//!
//! The byte-oriented drivers, [`AsyncIoDriver`] and [`OwnedBufDriver`], live here so byte
//! backends and apps share one contract. Backends may define their own drivers, for example
//! one whose reader and writer hand out typed values for a structured store.
use crate::Store;
use futures::future::Future;
use std::ops::Deref;

pub mod async_io;
pub mod owned_buf;

/// Bundles the types for one style of I/O. `Vault`, `Claim` and `Store` only pass these
/// types through, so a driver decides what a reader and a writer are without changes to
/// the core library.
pub trait IoDriver: Sized + 'static {
    /// The type to use for reading data from a vault object.
    type Reader<'a>;

    /// The type to use for writing data to a vault object.
    type Writer<'a>;

    /// The type to use as a future for async operations on the `Store`.
    type Fut<'a, T: 'a>: Future<Output = T> + 'a;

    /// The type to use as a pointer for the dynamic dispatch `Store`.
    type StorePtr: Clone + Deref<Target: Store<Self>>;
}

/// Turns a `Store` instance into the driver's `IoDriver::StorePtr`. Implemented by the
/// driver (`impl<S> IntoStorePtr<S> for MyDriver`) so that drivers outside this crate can
/// implement it: the reverse form, `impl<S> IntoStorePtr<MyDriver> for S`, breaks the orphan
/// rule.
pub trait IntoStorePtr<S>: IoDriver {
    fn into_store_ptr(store: S) -> Self::StorePtr;
}

pub use async_io::AsyncIoDriver;
pub use owned_buf::OwnedBufDriver;
