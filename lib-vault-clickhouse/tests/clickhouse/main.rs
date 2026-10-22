//! Integration tests for `lib-vault-clickhouse` against a live ClickHouse server with Keeper.
//! Built only with the `integration-tests` feature:
//!
//! ```text
//! docker compose -f lib-vault-clickhouse/tests/docker/compose.yaml up -d --wait
//! cargo test -p lib-vault-clickhouse --features integration-tests
//! ```
//!
//! `LIB_VAULT_CLICKHOUSE_URL` (default `http://localhost:8123`) and
//! `LIB_VAULT_CLICKHOUSE_PASSWORD` (default `vault`, as in the compose file) select the server.
//!
//! One test binary: `app` is the example application every test stores (its record types and
//! its `Dispatcher`). It's app code, the part a user of this crate writes, not a test helper,
//! so the tests share it (D76). The connection plumbing below sets up the environment each
//! test runs in; it doesn't wrap any vault API.

mod app;
mod claims;
mod objects;

use clickhouse::Client;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A client for the server under test, on its default database.
pub fn server() -> Client {
    Client::default()
        .with_url(
            std::env::var("LIB_VAULT_CLICKHOUSE_URL")
                .unwrap_or_else(|_| "http://localhost:8123".to_owned()),
        )
        .with_user("default")
        .with_password(
            std::env::var("LIB_VAULT_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "vault".to_owned()),
        )
}

/// A new, empty database for one test, and a client using it. Its name also scopes the
/// test's Keeper paths, so tests running in parallel never share tables or claims.
pub async fn fresh_database() -> (Client, String) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!(
        "t_{}_{nanos}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    server()
        .query(&format!("CREATE DATABASE {name}"))
        .execute()
        .await
        .expect("is the ClickHouse test server running? see tests/clickhouse/main.rs");
    let client = server().with_database(&name);
    (client, name)
}
