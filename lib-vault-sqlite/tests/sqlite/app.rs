//! An example application: two record types and the `Dispatcher` that maps them onto tables.
//!
//! - `objects (key, kind)` indexes every stored key. Its case-insensitive unique index
//!   enforces the case rule (D6), and `list` and `delete` only need this table.
//! - `docs` and `users` hold one row per value, in typed columns, and are removed with their
//!   `objects` row (`ON DELETE CASCADE`; sqlx turns foreign keys on by default).

use lib_vault::Key;
use lib_vault_sqlite::{DispatchError, Dispatcher};
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;

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

pub struct AppDispatcher;

impl Dispatcher for AppDispatcher {
    type Value = AppValue;

    async fn migrate(&self, conn: &mut SqliteConnection) -> Result<(), DispatchError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS objects (key TEXT PRIMARY KEY, kind TEXT NOT NULL)",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS objects_nocase ON objects (key COLLATE NOCASE)",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS docs (
                key TEXT PRIMARY KEY REFERENCES objects (key) ON DELETE CASCADE,
                title TEXT NOT NULL,
                body TEXT NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                key TEXT PRIMARY KEY REFERENCES objects (key) ON DELETE CASCADE,
                name TEXT NOT NULL,
                age INTEGER NOT NULL
            )",
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn read(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
    ) -> Result<Option<AppValue>, DispatchError> {
        let Some(object) = sqlx::query("SELECT kind FROM objects WHERE key = ?")
            .bind(key.as_str())
            .fetch_optional(&mut *conn)
            .await?
        else {
            return Ok(None);
        };
        let kind: String = object.get("kind");
        let value = match kind.as_str() {
            "doc" => {
                let row = sqlx::query("SELECT title, body FROM docs WHERE key = ?")
                    .bind(key.as_str())
                    .fetch_one(&mut *conn)
                    .await?;
                AppValue::Doc(Doc {
                    title: row.get("title"),
                    body: row.get("body"),
                })
            }
            "user" => {
                let row = sqlx::query("SELECT name, age FROM users WHERE key = ?")
                    .bind(key.as_str())
                    .fetch_one(&mut *conn)
                    .await?;
                AppValue::User(User {
                    name: row.get("name"),
                    age: row.get("age"),
                })
            }
            other => return Err(format!("unknown kind {other:?} at {key}").into()),
        };
        Ok(Some(value))
    }

    async fn write(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
        value: AppValue,
    ) -> Result<(), DispatchError> {
        // Replacing the `objects` row drops the old typed row, even of another kind.
        sqlx::query("DELETE FROM objects WHERE key = ?")
            .bind(key.as_str())
            .execute(&mut *conn)
            .await?;
        let kind = match value {
            AppValue::Doc(_) => "doc",
            AppValue::User(_) => "user",
        };
        // Fails on the case-insensitive unique index if `key` differs only by case from an
        // existing key.
        sqlx::query("INSERT INTO objects (key, kind) VALUES (?, ?)")
            .bind(key.as_str())
            .bind(kind)
            .execute(&mut *conn)
            .await?;
        match value {
            AppValue::Doc(doc) => {
                sqlx::query("INSERT INTO docs (key, title, body) VALUES (?, ?, ?)")
                    .bind(key.as_str())
                    .bind(doc.title)
                    .bind(doc.body)
                    .execute(&mut *conn)
                    .await?;
            }
            AppValue::User(user) => {
                sqlx::query("INSERT INTO users (key, name, age) VALUES (?, ?, ?)")
                    .bind(key.as_str())
                    .bind(user.name)
                    .bind(user.age)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        Ok(())
    }

    async fn delete(&self, conn: &mut SqliteConnection, key: &Key) -> Result<(), DispatchError> {
        sqlx::query("DELETE FROM objects WHERE key = ?")
            .bind(key.as_str())
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn list(
        &self,
        conn: &mut SqliteConnection,
        key: &Key,
    ) -> Result<Vec<Key>, DispatchError> {
        // Component-wise: `dir` matches `dir/x`, not `dirt/x` or `dir` itself (D40).
        let rows = sqlx::query(
            "SELECT key FROM objects WHERE substr(key, 1, length(?) + 1) = ? || '/' ORDER BY key",
        )
        .bind(key.as_str())
        .bind(key.as_str())
        .fetch_all(&mut *conn)
        .await?;
        let mut keys = Vec::new();
        for row in rows {
            let full = Key::new(row.get("key"))?;
            keys.extend(full.strip_prefix(key));
        }
        Ok(keys)
    }
}
