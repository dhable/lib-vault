use futures::future::Future;

pub mod owned;
pub mod shared;

mod private {
    // Exposed in a module that can only be accessed from this module and its children
    // through the `super::` construct. This prevents outside code from the core library
    // from extending the `IoMode`.
    pub trait Sealed {}
}

/// Trait used solely as a container for bundling a series of types used in the core
/// library without binding tightly to a specific implementation.
pub trait IoMode: private::Sealed + 'static {
    /// The type to use for reading data from a vault object.
    type Reader<'a>;

    /// The type to use for writing data to a vault object.
    type Writer<'a>;

    /// The type to use as a future for async operations on the `Store`.
    type Fut<'a, T: 'a>: Future<Output = T> + 'a;
}

pub use owned::OwnedMode;
pub use shared::SharedMode;
