//! A filesystem backend for `lib-vault` on Linux (≥ 5.6) and macOS (≥ 15): an [`FsStore`]
//! that keeps objects as files under a vault root, and the [`FsClaimer`] that locks them with
//! `flock` across threads and processes.
//!
//! **Requires a tokio runtime.** Choosing this backend means the app runs on tokio (D56): its
//! futures, readers and writers run blocking system calls on tokio's blocking pool and panic if
//! polled outside a tokio runtime. `lib-vault` itself and other backends don't need tokio.
//!
//! ```no_run
//! use futures::{AsyncReadExt, AsyncWriteExt};
//! use lib_vault::Key;
//! use lib_vault_posix::FsStoreOptions;
//!
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let vault = lib_vault_posix::vault("/srv/app/vault", FsStoreOptions::default())?;
//! let mut claim = vault.claim(Key::new("session")?).await?;
//!
//! let mut writer = claim.open_write("notes.md").await?;
//! writer.write_all(b"hello").await?;
//! writer.close().await?; // the commit point
//! drop(writer);
//!
//! let mut text = String::new();
//! claim.open_read("notes.md").await?.read_to_string(&mut text).await?;
//! claim.release()?;
//! # Ok(()) }
//! ```
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

use lib_vault::{Vault, rt::SharedMode};
use std::path::Path;

/// A `Vault` over an [`FsStore`] at `root` and its paired [`FsClaimer`].
pub fn vault(
    root: impl AsRef<Path>,
    options: FsStoreOptions,
) -> Result<Vault<SharedMode>, PosixError> {
    let store = FsStore::new(root, options)?;
    let claimer = store.claimer();
    Ok(Vault::new(store, claimer))
}
