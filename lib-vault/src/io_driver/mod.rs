//! I/O drivers: the future, object-pointer and store-pointer types that `Vault`, `Claim` and
//! `Store` are generic over.
//!
//! The general-purpose drivers, [`AsyncIoDriver`] and [`OwnedBufDriver`], live here so
//! backends and apps share one contract. Both pass objects as type-erased `Any` pointers.
//! Backends may define their own drivers, for example one whose object pointer is the typed
//! value of a structured store.
use crate::Store;
use futures::future::Future;
use std::any::Any;
use std::ops::Deref;

pub mod async_io;
pub mod owned_buf;

/// Bundles the types for one style of I/O. `Vault`, `Claim` and `Store` only pass these
/// types through, so a driver decides how objects travel between a `Claim` and its `Store`
/// without changes to the core library.
pub trait IoDriver: Sized + 'static {
    /// The type to use as a future for async operations on the `Store`.
    type Fut<'a, T: 'a>: Future<Output = T> + 'a;

    /// The type that carries a whole object through `Store::load` and `Store::store`.
    /// `Claim::load` and `Claim::store` convert it to and from the caller's type with
    /// [`IntoObjectPtr`].
    type ObjectPtr;

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

/// Converts a value of type `T` into the driver's `IoDriver::ObjectPtr` and back. Implemented
/// by the driver for every `T` it can carry, like [`IntoStorePtr`].
pub trait IntoObjectPtr<T: Any>: IoDriver {
    /// Wraps `obj` for `Store::store`.
    fn into_object_ptr(obj: T) -> Self::ObjectPtr;

    /// Takes the `T` out of a pointer returned by `Store::load`. `None` if the pointer doesn't
    /// hold a `T`, or if the store kept another reference to a shared pointer; `Claim::load`
    /// reports either as `Error::WrongType`.
    fn from_object_ptr(ptr: Self::ObjectPtr) -> Option<T>;
}

pub use async_io::AsyncIoDriver;
pub use owned_buf::OwnedBufDriver;
