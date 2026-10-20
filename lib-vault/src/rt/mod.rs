use crate::Store;
use futures::future::Future;
use std::ops::Deref;

pub mod owned;
pub mod shared;

/// Trait used solely as a container for bundling a series of types used in the core
/// library without binding tightly to a specific implementation.
pub trait IoMode: Sized + 'static {
    /// The type to use for reading data from a vault object.
    type Reader<'a>;

    /// The type to use for writing data to a vault object.
    type Writer<'a>;

    /// The type to use as a future for async operations on the `Store`.
    type Fut<'a, T: 'a>: Future<Output = T> + 'a;

    /// The type to use as a pointer for the dynamic dispatch `Store`.
    type StorePtr: Clone + Deref<Target: Store<Self>>;
}

/// Trait that can turn a `Store` instance into a `IoMode::StorePtr` based
/// on the individual `IoMode` types.
pub trait IntoStorePtr<M: IoMode> {
    fn into_store_ptr(self) -> M::StorePtr;
}

pub use owned::OwnedMode;
pub use shared::SharedMode;
