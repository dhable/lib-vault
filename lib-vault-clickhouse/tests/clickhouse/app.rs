//! An example application: two record types and the `Dispatcher` that maps them onto
//! ClickHouse tables.
//!
//! - `objects (lkey, key, kind)`, a `KeeperMap` table keyed by the lowercased key, indexes
//!   every stored key. Strict-mode inserts make the case rule (D6) atomic: a key that differs
//!   only by case finds its `lkey` taken. `list` only needs this table.
//! - `docs` and `users` hold the values in typed columns as `ReplacingMergeTree(version,
//!   is_deleted)` tables: a write inserts a newer row, a delete inserts a tombstone, and
//!   `FINAL` reads see only the latest live row per key.

use clickhouse::{Client, Row};
use lib_vault::Key;
use lib_vault_clickhouse::{DispatchError, Dispatcher};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub name: String,
    pub age: i64,
}

/// Everything the vault stores: a variant per record type.
#[derive(Clone, Debug, PartialEq)]
pub enum AppValue {
    Doc(Doc),
    User(User),
}

impl From<Doc> for AppValue {
    fn from(doc: Doc) -> Self {
        AppValue::Doc(doc)
    }
}

impl From<User> for AppValue {
    fn from(user: User) -> Self {
        AppValue::User(user)
    }
}

impl TryFrom<AppValue> for Doc {
    type Error = AppValue;
    fn try_from(value: AppValue) -> Result<Self, AppValue> {
        match value {
            AppValue::Doc(doc) => Ok(doc),
            other => Err(other),
        }
    }
}

impl TryFrom<AppValue> for User {
    type Error = AppValue;
    fn try_from(value: AppValue) -> Result<Self, AppValue> {
        match value {
            AppValue::User(user) => Ok(user),
            other => Err(other),
        }
    }
}

/// Raised by `write` when the key differs only by case from a stored one (D6).
#[derive(Debug, thiserror::Error)]
#[error("{key} differs only by case from the stored key {existing}")]
pub struct CaseCollision {
    pub key: String,
    pub existing: String,
}

#[derive(Row, Serialize, Deserialize)]
struct ObjectRow {
    lkey: String,
    key: String,
    kind: String,
}

#[derive(Row, Serialize, Deserialize)]
struct DocRow {
    key: String,
    title: String,
    body: String,
    version: u64,
    is_deleted: u8,
}

#[derive(Row, Serialize, Deserialize)]
struct UserRow {
    key: String,
    name: String,
    age: i64,
    version: u64,
    is_deleted: u8,
}

pub struct AppDispatcher {
    // Where the `objects` index lives in Keeper.
    keeper_path: String,
    // The last row version handed out: newer rows must win in `ReplacingMergeTree`.
    last_version: AtomicU64,
}

impl AppDispatcher {
    pub fn new(keeper_path: impl Into<String>) -> Self {
        Self {
            keeper_path: keeper_path.into(),
            last_version: AtomicU64::new(0),
        }
    }

    // A row version greater than every earlier one from this dispatcher.
    fn next_version(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let mut last = self.last_version.load(Ordering::Relaxed);
        loop {
            let next = now.max(last + 1);
            match self.last_version.compare_exchange(
                last,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return next,
                Err(seen) => last = seen,
            }
        }
    }

    async fn object(&self, client: &Client, key: &Key) -> Result<Option<ObjectRow>, DispatchError> {
        Ok(client
            .query("SELECT ?fields FROM objects WHERE lkey = ?")
            .bind(key.as_str().to_ascii_lowercase())
            .fetch_optional::<ObjectRow>()
            .await?
            .filter(|row| row.key == key.as_str()))
    }

    // Hides `key`'s row in the table for `kind`.
    async fn tombstone(&self, client: &Client, key: &Key, kind: &str) -> Result<(), DispatchError> {
        let version = self.next_version();
        match kind {
            "doc" => {
                let mut insert = client.insert::<DocRow>("docs").await?;
                insert
                    .write(&DocRow {
                        key: key.as_str().to_owned(),
                        title: String::new(),
                        body: String::new(),
                        version,
                        is_deleted: 1,
                    })
                    .await?;
                insert.end().await?;
            }
            "user" => {
                let mut insert = client.insert::<UserRow>("users").await?;
                insert
                    .write(&UserRow {
                        key: key.as_str().to_owned(),
                        name: String::new(),
                        age: 0,
                        version,
                        is_deleted: 1,
                    })
                    .await?;
                insert.end().await?;
            }
            other => return Err(format!("unknown kind {other:?} at {key}").into()),
        }
        Ok(())
    }
}

impl Dispatcher for AppDispatcher {
    type Value = AppValue;

    async fn migrate(&self, client: &Client) -> Result<(), DispatchError> {
        client
            .query(
                "CREATE TABLE IF NOT EXISTS objects (lkey String, key String, kind String)
                 ENGINE = KeeperMap(?) PRIMARY KEY lkey",
            )
            .bind(format!("{}/objects", self.keeper_path))
            .execute()
            .await?;
        client
            .query(
                "CREATE TABLE IF NOT EXISTS docs
                    (key String, title String, body String, version UInt64, is_deleted UInt8)
                 ENGINE = ReplacingMergeTree(version, is_deleted) ORDER BY key",
            )
            .execute()
            .await?;
        client
            .query(
                "CREATE TABLE IF NOT EXISTS users
                    (key String, name String, age Int64, version UInt64, is_deleted UInt8)
                 ENGINE = ReplacingMergeTree(version, is_deleted) ORDER BY key",
            )
            .execute()
            .await?;
        Ok(())
    }

    async fn read(&self, client: &Client, key: &Key) -> Result<Option<AppValue>, DispatchError> {
        let Some(object) = self.object(client, key).await? else {
            return Ok(None);
        };
        let value = match object.kind.as_str() {
            "doc" => client
                .query("SELECT ?fields FROM docs FINAL WHERE key = ?")
                .bind(key.as_str())
                .fetch_optional::<DocRow>()
                .await?
                .map(|row| {
                    AppValue::Doc(Doc {
                        title: row.title,
                        body: row.body,
                    })
                }),
            "user" => client
                .query("SELECT ?fields FROM users FINAL WHERE key = ?")
                .bind(key.as_str())
                .fetch_optional::<UserRow>()
                .await?
                .map(|row| {
                    AppValue::User(User {
                        name: row.name,
                        age: row.age,
                    })
                }),
            other => return Err(format!("unknown kind {other:?} at {key}").into()),
        };
        Ok(value)
    }

    async fn write(
        &self,
        client: &Client,
        key: &Key,
        value: AppValue,
    ) -> Result<(), DispatchError> {
        let kind = match &value {
            AppValue::Doc(_) => "doc",
            AppValue::User(_) => "user",
        };
        let lkey = key.as_str().to_ascii_lowercase();

        // Index first: refuses a case collision before any data is written.
        let existing = client
            .query("SELECT ?fields FROM objects WHERE lkey = ?")
            .bind(&lkey)
            .fetch_optional::<ObjectRow>()
            .await?;
        match existing {
            Some(row) if row.key != key.as_str() => {
                return Err(Box::new(CaseCollision {
                    key: key.as_str().to_owned(),
                    existing: row.key,
                }));
            }
            Some(row) if row.kind != kind => {
                self.tombstone(client, key, &row.kind).await?;
                client
                    .query("ALTER TABLE objects UPDATE kind = ? WHERE lkey = ?")
                    .with_setting("keeper_map_strict_mode", "1")
                    .bind(kind)
                    .bind(&lkey)
                    .execute()
                    .await?;
            }
            Some(_) => {}
            None => {
                // Strict mode: fails if another key with the same lowercase spelling got
                // there first.
                let inserted = client
                    .query("INSERT INTO objects VALUES (?, ?, ?)")
                    .with_setting("keeper_map_strict_mode", "1")
                    .bind(&lkey)
                    .bind(key.as_str())
                    .bind(kind)
                    .execute()
                    .await;
                if let Err(err) = inserted {
                    let winner = client
                        .query("SELECT ?fields FROM objects WHERE lkey = ?")
                        .bind(&lkey)
                        .fetch_optional::<ObjectRow>()
                        .await?;
                    match winner {
                        Some(row) if row.key != key.as_str() => {
                            return Err(Box::new(CaseCollision {
                                key: key.as_str().to_owned(),
                                existing: row.key,
                            }));
                        }
                        Some(_) => {}
                        None => return Err(err.into()),
                    }
                }
            }
        }

        let version = self.next_version();
        match value {
            AppValue::Doc(doc) => {
                let mut insert = client.insert::<DocRow>("docs").await?;
                insert
                    .write(&DocRow {
                        key: key.as_str().to_owned(),
                        title: doc.title,
                        body: doc.body,
                        version,
                        is_deleted: 0,
                    })
                    .await?;
                insert.end().await?;
            }
            AppValue::User(user) => {
                let mut insert = client.insert::<UserRow>("users").await?;
                insert
                    .write(&UserRow {
                        key: key.as_str().to_owned(),
                        name: user.name,
                        age: user.age,
                        version,
                        is_deleted: 0,
                    })
                    .await?;
                insert.end().await?;
            }
        }
        Ok(())
    }

    async fn delete(&self, client: &Client, key: &Key) -> Result<(), DispatchError> {
        let Some(object) = self.object(client, key).await? else {
            return Ok(()); // like HTTP DELETE
        };
        self.tombstone(client, key, &object.kind).await?;
        client
            .query("DELETE FROM objects WHERE lkey = ?")
            .bind(object.lkey)
            .execute()
            .await?;
        Ok(())
    }

    async fn list(&self, client: &Client, key: &Key) -> Result<Vec<Key>, DispatchError> {
        // Component-wise: `dir` matches `dir/x`, not `dirt/x` or `dir` itself (D40).
        let keys = client
            .query("SELECT key FROM objects WHERE startsWith(key, ?) ORDER BY key")
            .bind(format!("{}/", key.as_str()))
            .fetch_all::<String>()
            .await?;
        let mut relative = Vec::new();
        for full in keys {
            relative.extend(Key::new(&full)?.strip_prefix(key));
        }
        Ok(relative)
    }
}
