mod claim;
mod claimer;
mod error;
pub mod io_driver;
mod key;
mod store;
mod vault;

pub use claim::*;
pub use claimer::*;
pub use error::*;
pub use key::*;
pub use store::*;
pub use vault::*;

// Compile and run the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
