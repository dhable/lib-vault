//! A SQLite backend for `lib-vault` that stores the app's own typed values rather than bytes:
//! a [`SqliteStore`], its I/O driver [`SqliteDriver`], and a [`SqliteClaimer`].
//!
//! The app supplies a [`Dispatcher`]: one value type for the vault (usually an enum with a
//! variant per struct) and the code that reads, writes, deletes and lists those values on a
//! SQLite connection. The store calls it for every operation, and `Claim` passes the typed
//! handles through untouched.
//!
//! Built on sqlx, so it requires the app to run on a **tokio** runtime (like `lib-vault-posix`,
//! D56).
//!
//! ```
//! use lib_vault::{Key, Vault};
//! use lib_vault_sqlite::{DispatchError, Dispatcher, SqliteClaimer, SqliteDriver, SqliteStore};
//! use sqlx::Row;
//! use sqlx::sqlite::{SqliteConnection, SqlitePool, SqlitePoolOptions};
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
//! // Maps `AppValue` onto a `docs` table. The case-insensitive unique index enforces the
//! // case rule every store must (D6).
//! struct AppDispatcher;
//!
//! impl Dispatcher for AppDispatcher {
//!     type Value = AppValue;
//!
//!     async fn migrate(&self, conn: &mut SqliteConnection) -> Result<(), DispatchError> {
//!         sqlx::query("CREATE TABLE IF NOT EXISTS docs (key TEXT PRIMARY KEY, title TEXT NOT NULL)")
//!             .execute(&mut *conn)
//!             .await?;
//!         sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS docs_nocase ON docs (key COLLATE NOCASE)")
//!             .execute(&mut *conn)
//!             .await?;
//!         Ok(())
//!     }
//!
//!     async fn read(&self, conn: &mut SqliteConnection, key: &Key) -> Result<Option<AppValue>, DispatchError> {
//!         let row = sqlx::query("SELECT title FROM docs WHERE key = ?")
//!             .bind(key.as_str())
//!             .fetch_optional(&mut *conn)
//!             .await?;
//!         Ok(row.map(|row| AppValue::Doc(MyDataObj { title: row.get("title") })))
//!     }
//!
//!     async fn write(&self, conn: &mut SqliteConnection, key: &Key, value: AppValue) -> Result<(), DispatchError> {
//!         let AppValue::Doc(doc) = value;
//!         sqlx::query(
//!             "INSERT INTO docs (key, title) VALUES (?, ?)
//!              ON CONFLICT (key) DO UPDATE SET title = excluded.title",
//!         )
//!         .bind(key.as_str())
//!         .bind(doc.title)
//!         .execute(&mut *conn)
//!         .await?;
//!         Ok(())
//!     }
//!
//!     async fn delete(&self, conn: &mut SqliteConnection, key: &Key) -> Result<(), DispatchError> {
//!         sqlx::query("DELETE FROM docs WHERE key = ?")
//!             .bind(key.as_str())
//!             .execute(&mut *conn)
//!             .await?;
//!         Ok(())
//!     }
//!
//!     async fn list(&self, conn: &mut SqliteConnection, key: &Key) -> Result<Vec<Key>, DispatchError> {
//!         // Component-wise: `a` matches `a/b`, not `ab`.
//!         let rows = sqlx::query("SELECT key FROM docs WHERE substr(key, 1, length(?) + 1) = ? || '/'")
//!             .bind(key.as_str())
//!             .bind(key.as_str())
//!             .fetch_all(&mut *conn)
//!             .await?;
//!         let mut keys = Vec::new();
//!         for row in rows {
//!             let full = Key::new(row.get("key"))?;
//!             keys.extend(full.strip_prefix(key));
//!         }
//!         Ok(keys)
//!     }
//! }
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // One connection keeps the in-memory database alive; use a file in real apps.
//! let pool: SqlitePool = SqlitePoolOptions::new()
//!     .max_connections(1)
//!     .idle_timeout(None)
//!     .max_lifetime(None)
//!     .connect("sqlite::memory:")
//!     .await?;
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
//!
//! Handles borrow the claim, so a claim can't be released while one is open (D42):
//!
//! ```compile_fail
//! # use lib_vault::Claim;
//! # use lib_vault_sqlite::{Dispatcher, SqliteDriver};
//! # async fn f<D: Dispatcher>(mut claim: Claim<SqliteDriver<D>>) {
//! let reader = claim.open_read(None).await.unwrap();
//! claim.release().unwrap(); // error: `claim` is still borrowed by `reader`
//! drop(reader);
//! # }
//! ```

mod claimer;
mod dispatch;
mod driver;
mod error;
mod handles;
mod store;

pub use claimer::{SqliteClaimer, SqliteClaimerOptions};
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

        async fn migrate(&self, _conn: &mut sqlx::SqliteConnection) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn read(
            &self,
            _conn: &mut sqlx::SqliteConnection,
            _key: &Key,
        ) -> Result<Option<()>, DispatchError> {
            unimplemented!()
        }
        async fn write(
            &self,
            _conn: &mut sqlx::SqliteConnection,
            _key: &Key,
            _value: (),
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn delete(
            &self,
            _conn: &mut sqlx::SqliteConnection,
            _key: &Key,
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn list(
            &self,
            _conn: &mut sqlx::SqliteConnection,
            _key: &Key,
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
