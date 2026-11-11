//! An in-memory backend for `lib-vault`: a [`MemoryStore`] and a [`MemoryClaimer`].
//!
//! Meant for tests and for getting started without any storage or platform dependencies,
//! not for production workloads. Everything lives in this process and is lost when the last
//! clone of the store is dropped.
//!
//! ```
//! use lib_vault::Key;
//!
//! # futures::executor::block_on(async {
//! let vault = lib_vault_memory::vault();
//! let mut claim = vault.claim(Key::new("session").unwrap()).await.unwrap();
//!
//! claim.store("notes.md", b"hello".to_vec()).await.unwrap();
//!
//! let bytes: Vec<u8> = claim.load("notes.md").await.unwrap();
//! assert_eq!(bytes, b"hello");
//! # });
//! ```

mod claimer;
mod store;

pub use claimer::MemoryClaimer;
pub use store::MemoryStore;

// Compile and run the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

use lib_vault::{Key, Vault, io_driver};

/// A `Vault` backed by a new [`MemoryStore`] and [`MemoryClaimer`].
pub fn vault() -> Vault<io_driver::AsyncIoDriver> {
    Vault::new(MemoryStore::new(), MemoryClaimer::new())
}

/// Errors specific to the memory backend. They reach callers wrapped in
/// `lib_vault::Error::StoreError`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("{key} differs only by case from the existing object {existing}")]
    CaseCollision { key: Key, existing: Key },
}

impl From<MemoryError> for lib_vault::Error {
    fn from(e: MemoryError) -> Self {
        lib_vault::Error::StoreError(Box::new(e))
    }
}
