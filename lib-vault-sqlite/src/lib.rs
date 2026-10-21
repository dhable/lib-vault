//! A SQLite backend for `lib-vault` that stores the app's own typed values rather than bytes:
//! a [`SqliteStore`], its I/O driver [`SqliteDriver`], and a [`SqliteClaimer`].
//!
//! **Status: API sketch.** Every method is `todo!()`; nothing talks to a database yet.
//!
//! The app supplies a [`Dispatcher`]: one value type for the vault (usually an enum with a
//! variant per struct) and the code that reads, writes, deletes and lists those values on a
//! SQLite connection. The store calls it for every operation, and `Claim` passes the typed
//! handles through untouched.
//!
//! Built on sqlx, so it requires the app to run on a **tokio** runtime (like `lib-vault-posix`,
//! D56).
//!
//! ```no_run
//! use lib_vault::{Key, Vault};
//! use lib_vault_sqlite::{DispatchError, Dispatcher, SqliteClaimer, SqliteDriver, SqliteStore};
//! use sqlx::sqlite::{SqliteConnection, SqlitePool};
//!
//! struct MyDataObj {
//!     title: String,
//! }
//!
//! // The one value type this vault stores: a variant per struct.
//! enum AppValue {
//!     Doc(MyDataObj),
//! }
//!
//! impl From<MyDataObj> for AppValue {
//!     fn from(doc: MyDataObj) -> Self {
//!         AppValue::Doc(doc)
//!     }
//! }
//!
//! impl TryFrom<AppValue> for MyDataObj {
//!     type Error = AppValue;
//!     fn try_from(value: AppValue) -> Result<Self, AppValue> {
//!         match value {
//!             AppValue::Doc(doc) => Ok(doc),
//!         }
//!     }
//! }
//!
//! // Maps `AppValue` onto tables; the queries are the app's choice.
//! struct AppDispatcher;
//!
//! impl Dispatcher for AppDispatcher {
//!     type Value = AppValue;
//!
//!     async fn migrate(&self, conn: &mut SqliteConnection) -> Result<(), DispatchError> {
//!         todo!("CREATE TABLE IF NOT EXISTS docs (key TEXT PRIMARY KEY, title TEXT)")
//!     }
//!     async fn read(&self, conn: &mut SqliteConnection, key: &Key) -> Result<Option<AppValue>, DispatchError> {
//!         todo!("SELECT title FROM docs WHERE key = ?")
//!     }
//!     async fn write(&self, conn: &mut SqliteConnection, key: &Key, value: AppValue) -> Result<(), DispatchError> {
//!         todo!("INSERT OR REPLACE INTO docs ...")
//!     }
//!     async fn delete(&self, conn: &mut SqliteConnection, key: &Key) -> Result<(), DispatchError> {
//!         todo!("DELETE FROM docs WHERE key = ?")
//!     }
//!     async fn list(&self, conn: &mut SqliteConnection, key: &Key) -> Result<Vec<Key>, DispatchError> {
//!         todo!("SELECT key FROM docs WHERE key LIKE ? || '/%'")
//!     }
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let pool = SqlitePool::connect("sqlite:app.db").await?;
//! let vault: Vault<SqliteDriver<AppDispatcher>> = Vault::new(
//!     SqliteStore::new(pool.clone(), AppDispatcher).await?,
//!     SqliteClaimer::new(pool).await?,
//! );
//! let mut claim = vault.claim(Key::new("docs/a")?).await?;
//!
//! let mut writer = claim.open_write(None).await?;
//! writer.put(MyDataObj { title: "hello".into() });
//! writer.close().await?; // commits
//!
//! let reader = claim.open_read(None).await?;
//! let doc: MyDataObj = reader.try_as()?;
//! assert_eq!(doc.title, "hello");
//! # Ok(())
//! # }
//! ```

// API sketch: the bodies are `todo!()`, so fields and arguments are unused. Remove when
// implemented.
#![allow(dead_code, unused_variables)]

mod claimer;
mod dispatch;
mod driver;
mod error;
mod handles;
mod store;

pub use claimer::SqliteClaimer;
pub use dispatch::{DispatchError, Dispatcher};
pub use driver::SqliteDriver;
pub use error::SqliteError;
pub use handles::{SqliteReader, SqliteWriter};
pub use store::SqliteStore;

#[cfg(test)]
mod tests {
    use super::*;
    use lib_vault::{Claim, Key, Vault};

    // Empty `Dispatcher` implementation that is used for test cases
    struct NoDispatcher;
    impl Dispatcher for NoDispatcher {
        type Value = ();

        async fn migrate(&self, conn: &mut sqlx::SqliteConnection) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn read(
            &self,
            conn: &mut sqlx::SqliteConnection,
            key: &Key,
        ) -> Result<Option<()>, DispatchError> {
            unimplemented!()
        }
        async fn write(
            &self,
            conn: &mut sqlx::SqliteConnection,
            key: &Key,
            value: (),
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn delete(
            &self,
            conn: &mut sqlx::SqliteConnection,
            key: &Key,
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn list(
            &self,
            conn: &mut sqlx::SqliteConnection,
            key: &Key,
        ) -> Result<Vec<Key>, DispatchError> {
            unimplemented!()
        }
    }

    #[test]
    fn send_marker() {
        // Compile-time guard: a SQLite vault, its claims and their futures stay `Send`, so
        // they can be used from tasks on tokio's multi-threaded runtime.
        fn assert_send<T: Send>() {}
        fn assert_vault<T: Send + Sync + Clone>() {}
        fn assert_send_value<T: Send>(_: &T) {}

        fn checks<D: Dispatcher>(vault: Vault<SqliteDriver<D>>) {
            assert_send::<Claim<SqliteDriver<D>>>();
            assert_vault::<Vault<SqliteDriver<D>>>();
            assert_send_value(&vault.claim(Key::new("a").unwrap()));
            assert_send_value(&vault.shutdown());
        }
        let _ = checks::<NoDispatcher>;
    }
}
