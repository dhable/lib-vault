//! A filesystem backend for `lib-vault` on Linux (≥ 5.6) and macOS (≥ 15): an [`FsStore`]
//! that keeps objects as files under a vault root, and an [`FsClaimer`] that locks them with
//! `flock` across threads and processes.
//!
//! **Requires a tokio runtime.** Choosing this backend means the app runs on tokio (D56): its
//! futures run blocking system calls on tokio's blocking pool and panic if
//! polled outside a tokio runtime. `lib-vault` itself and other backends don't need tokio.
//!
//! ```no_run
//! use lib_vault::{Key, Vault};
//! use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions};
//!
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = FsStore::new("/srv/app/vault", FsStoreOptions::default())?;
//! let claimer = FsClaimer::new(&store); // or any other `Claimer`
//! let vault = Vault::new(store, claimer);
//! let mut claim = vault.claim(Key::new("session")?).await?;
//!
//! claim.store("notes.md", b"hello".to_vec()).await?; // journaled, then committed
//!
//! let bytes: Vec<u8> = claim.load("notes.md").await?;
//! assert_eq!(bytes, b"hello");
//! claim.release()?;
//! # Ok(()) }
//! ```
//!
//! # Claimers
//!
//! The store and the claimer are chosen separately (D75). [`FsStore`] works with any
//! [`Claimer`](lib_vault::Claimer); [`FsClaimer`] works only with an `FsStore`:
//!
//! | Claimer | Excludes | Use when |
//! |---|---|---|
//! | [`FsClaimer`] | Processes on one host sharing the root (`flock`) | The vault is on a local filesystem |
//! | A distributed claimer (e.g. ZooKeeper) | Processes on every host that uses it | Hosts share the storage |
//! | `lib_vault_memory::MemoryClaimer` | Vaults in one process sharing the claimer | Tests |
//!
//! Every process using a vault root must use the same kind of claimer: different claimers
//! don't see each other's claims. This isn't detected.
//!
//! See `docs/design.md` §8–§9 for the protocol and checks.

mod acl;
mod claimer;
mod error;
mod fsops;
mod journal;
mod options;
mod root;
mod store;

pub use claimer::FsClaimer;
pub use error::PosixError;
pub use options::{FsStoreOptions, Sharing};
pub use store::FsStore;

// Compile and run the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
