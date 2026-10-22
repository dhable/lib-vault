//! A ClickHouse backend for `lib-vault` that stores the app's own typed values rather than
//! bytes: a [`ClickhouseStore`], its I/O driver [`ClickhouseDriver`], and a
//! [`ClickhouseClaimer`] that keeps claims in Keeper through `KeeperMap` tables.
//!
//! As with `lib-vault-sqlite`, the app supplies a [`Dispatcher`]: one value type for the
//! vault (usually an enum with a variant per struct) and the code that reads, writes,
//! deletes and lists those values with a [`clickhouse::Client`], in whatever table schema
//! suits them. The store calls it for every operation, and `Claim` passes the typed handles
//! through untouched.
//!
//! Requires a **tokio** runtime (the `clickhouse` client is built on hyper and tokio). The
//! claimer also needs the server to have Keeper configured (`<zookeeper>` and
//! `<keeper_map_path_prefix>`); see the README.
//!
//! ```no_run
//! use clickhouse::Client;
//! use lib_vault::{Key, Vault};
//! use lib_vault_clickhouse::{
//!     ClickhouseClaimer, ClickhouseDriver, ClickhouseStore, DispatchError, Dispatcher,
//! };
//!
//! #[derive(Clone, clickhouse::Row, serde::Serialize, serde::Deserialize)]
//! struct MyDataObj {
//!     title: String,
//! }
//!
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
//! // A `ReplacingMergeTree` keeps the latest row per key; deletes insert a tombstone. (A
//! // real app also enforces the case rule; see the `Dispatcher` docs.)
//! struct AppDispatcher;
//!
//! impl Dispatcher for AppDispatcher {
//!     type Value = AppValue;
//!
//!     async fn migrate(&self, client: &Client) -> Result<(), DispatchError> {
//!         client
//!             .query(
//!                 "CREATE TABLE IF NOT EXISTS docs
//!                     (key String, title String, version UInt64, is_deleted UInt8)
//!                  ENGINE = ReplacingMergeTree(version, is_deleted) ORDER BY key",
//!             )
//!             .execute()
//!             .await?;
//!         Ok(())
//!     }
//!
//!     async fn read(&self, client: &Client, key: &Key) -> Result<Option<AppValue>, DispatchError> {
//!         let title = client
//!             .query("SELECT title FROM docs FINAL WHERE key = ?")
//!             .bind(key.as_str())
//!             .fetch_optional::<String>()
//!             .await?;
//!         Ok(title.map(|title| AppValue::Doc(MyDataObj { title })))
//!     }
//!
//!     async fn write(&self, client: &Client, key: &Key, value: AppValue) -> Result<(), DispatchError> {
//!         let AppValue::Doc(doc) = value;
//!         client
//!             .query("INSERT INTO docs SELECT ?, ?, toUnixTimestamp64Nano(now64(9)), 0")
//!             .bind(key.as_str())
//!             .bind(doc.title)
//!             .execute()
//!             .await?;
//!         Ok(())
//!     }
//!
//!     async fn delete(&self, client: &Client, key: &Key) -> Result<(), DispatchError> {
//!         client
//!             .query("INSERT INTO docs SELECT ?, '', toUnixTimestamp64Nano(now64(9)), 1")
//!             .bind(key.as_str())
//!             .execute()
//!             .await?;
//!         Ok(())
//!     }
//!
//!     async fn list(&self, client: &Client, key: &Key) -> Result<Vec<Key>, DispatchError> {
//!         let keys = client
//!             .query("SELECT key FROM docs FINAL WHERE startsWith(key, ?)")
//!             .bind(format!("{}/", key.as_str()))
//!             .fetch_all::<String>()
//!             .await?;
//!         let mut relative = Vec::new();
//!         for full in keys {
//!             relative.extend(Key::new(&full)?.strip_prefix(key));
//!         }
//!         Ok(relative)
//!     }
//! }
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = Client::default()
//!     .with_url("http://localhost:8123")
//!     .with_user("default")
//!     .with_password("secret");
//! let vault: Vault<ClickhouseDriver<AppDispatcher>> = Vault::new(
//!     ClickhouseStore::new(client.clone(), AppDispatcher).await?,
//!     ClickhouseClaimer::new(client).await?,
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
//! # use lib_vault_clickhouse::{ClickhouseDriver, Dispatcher};
//! # async fn f<D: Dispatcher>(mut claim: Claim<ClickhouseDriver<D>>) {
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
mod protocol;
mod store;

pub use claimer::{ClickhouseClaimer, ClickhouseClaimerOptions};
pub use dispatch::{DispatchError, Dispatcher};
pub use driver::ClickhouseDriver;
pub use error::ClickhouseError;
pub use handles::{ClickhouseReader, ClickhouseWriter};
pub use store::ClickhouseStore;

#[cfg(test)]
mod tests {
    use super::*;
    use clickhouse::Client;
    use lib_vault::{Claim, Key, Vault};

    // A `Dispatcher` that is never called: the tests below only need its type.
    struct NoDispatcher;
    impl Dispatcher for NoDispatcher {
        type Value = ();

        async fn migrate(&self, _client: &Client) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn read(&self, _client: &Client, _key: &Key) -> Result<Option<()>, DispatchError> {
            unimplemented!()
        }
        async fn write(
            &self,
            _client: &Client,
            _key: &Key,
            _value: (),
        ) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn delete(&self, _client: &Client, _key: &Key) -> Result<(), DispatchError> {
            unimplemented!()
        }
        async fn list(&self, _client: &Client, _key: &Key) -> Result<Vec<Key>, DispatchError> {
            unimplemented!()
        }
    }

    #[test]
    fn send_marker() {
        // Compile-time guard: a ClickHouse vault, its claims and their futures stay `Send`,
        // and a task using a claim can be spawned on tokio's multi-threaded runtime.
        fn assert_send<T: Send>() {}
        fn assert_vault<T: Send + Sync + Clone>() {}
        fn assert_send_value<T: Send>(_: &T) {}
        fn spawnable<F: Future + Send + 'static>(_: F) {}

        fn checks<D: Dispatcher>(vault: Vault<ClickhouseDriver<D>>) {
            assert_send::<Claim<ClickhouseDriver<D>>>();
            assert_vault::<Vault<ClickhouseDriver<D>>>();
            assert_send_value(&vault.claim(Key::new("a").unwrap()));
            assert_send_value(&vault.clone().shutdown());
            spawnable(async move {
                let mut c = vault.claim(Key::new("a").unwrap()).await.unwrap();
                let _ = c.list(None).await;
                if let Ok(w) = c.open_write("x").await {
                    let _ = w.close().await;
                }
                drop(c.open_read("x").await);
            });
        }
        let _ = checks::<NoDispatcher>;
    }

    #[tokio::test]
    async fn invalid_claimer_options_are_refused_before_any_request() {
        // No server is running: the options are checked before the claimer connects.
        let client = Client::default().with_url("http://127.0.0.1:9");
        for root in ["relative", "/a//b", "/quote'"] {
            let result = ClickhouseClaimer::with_options(
                client.clone(),
                ClickhouseClaimerOptions::default().root_path(root),
            )
            .await;
            assert!(
                matches!(result, Err(ClickhouseError::InvalidOption(_))),
                "{root}"
            );
        }
        let result = ClickhouseClaimer::with_options(
            client,
            ClickhouseClaimerOptions::default().lease(std::time::Duration::ZERO),
        )
        .await;
        assert!(matches!(result, Err(ClickhouseError::InvalidOption(_))));
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_client_error() {
        // Nothing listens on port 9, so creating the claimer fails with the client's error
        // rather than hanging or panicking.
        let client = Client::default().with_url("http://127.0.0.1:9");
        let result = ClickhouseClaimer::new(client).await;
        assert!(matches!(result, Err(ClickhouseError::Client(_))));
    }
}
