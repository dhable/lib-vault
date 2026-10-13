use futures::future::Future;

pub mod owned;
pub mod shared;

mod private {
    pub trait Sealed {}
}

pub trait IoMode: private::Sealed + 'static {
    type Reader<'a>;
    type Writer<'a>;
    type Fut<'a, T: 'a>: Future<Output = T> + 'a;
}

pub use owned::OwnedMode;
pub use shared::SharedMode;
