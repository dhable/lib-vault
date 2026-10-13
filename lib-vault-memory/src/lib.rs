//! An in-memory backend for `lib-vault`: a [`MemoryStore`] and a [`MemoryClaimer`].
//!
//! Meant for tests and for getting started without any storage or platform dependencies,
//! not for production workloads. Everything lives in this process and is lost when the last
//! clone of the store is dropped.
//!
//! ```
//! use futures::{AsyncReadExt, AsyncWriteExt};
//! use lib_vault::Key;
//!
//! # futures::executor::block_on(async {
//! let vault = lib_vault_memory::vault();
//! let mut claim = vault.claim(Key::new("session").unwrap()).await.unwrap();
//!
//! let mut writer = claim.open_write("notes.md").await.unwrap();
//! writer.write_all(b"hello").await.unwrap();
//! writer.close().await.unwrap(); // the memory store commits on close
//! drop(writer);
//!
//! let mut reader = claim.open_read("notes.md").await.unwrap();
//! let mut text = String::new();
//! reader.read_to_string(&mut text).await.unwrap();
//! assert_eq!(text, "hello");
//! # });
//! ```
//!
//! Handles borrow the claim, so a claim can't be released while one is open:
//!
//! ```compile_fail
//! # use lib_vault::Key;
//! # futures::executor::block_on(async {
//! let vault = lib_vault_memory::vault();
//! let mut claim = vault.claim(Key::new("session").unwrap()).await.unwrap();
//! let reader = claim.open_read("notes.md").await.unwrap();
//! claim.release().unwrap(); // error: `claim` is still borrowed by `reader`
//! drop(reader);
//! # });
//! ```

mod claimer;
mod store;

pub use claimer::MemoryClaimer;
pub use store::MemoryStore;

use lib_vault::{Key, Vault, rt};
use std::io;

/// A `Vault` backed by a new [`MemoryStore`] and [`MemoryClaimer`].
pub fn vault() -> Vault<rt::SharedMode> {
    Vault::new(MemoryStore::new(), MemoryClaimer::new())
}

/// Errors specific to the memory backend. They reach callers wrapped in
/// `lib_vault::Error::StoreError`, or as an `io::Error` from a writer's `close()`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    #[error("{key} differs only by case from the existing object {existing}")]
    CaseCollision { key: Key, existing: Key },

    #[error("writer is already closed")]
    WriterClosed,
}

impl From<MemoryError> for lib_vault::Error {
    fn from(e: MemoryError) -> Self {
        lib_vault::Error::StoreError(Box::new(e))
    }
}

impl From<MemoryError> for io::Error {
    fn from(e: MemoryError) -> Self {
        let kind = match e {
            MemoryError::CaseCollision { .. } => io::ErrorKind::AlreadyExists,
            MemoryError::WriterClosed => io::ErrorKind::BrokenPipe,
        };
        io::Error::new(kind, e)
    }
}
