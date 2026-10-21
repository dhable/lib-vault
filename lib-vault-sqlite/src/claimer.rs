use crate::SqliteError;
use futures::future::BoxFuture;
use lib_vault::{ClaimLock, Claimer, Error, Key};
use sqlx::sqlite::SqlitePool;

/// A `Claimer` that records claims in a SQLite table, `vault_claims(key TEXT PRIMARY KEY,
/// owner TEXT, acquired_at INTEGER)`, so processes sharing the database file exclude each
/// other. Works with any store.
///
/// `try_lock` runs one `BEGIN IMMEDIATE` transaction that refuses the key if a row holds the
/// same key, an ancestor or a descendant (D44), and otherwise inserts a row.
///
/// **Open problems** (not yet designed):
/// - **Release** must be synchronous and non-blocking, and `Ok` must mean the key can be
///   claimed again immediately (D39). Deleting the row is async sqlx work.
/// - **Crashed holders** leave their rows behind and block the key forever, so rows need an
///   owner identity plus a lease or stale-row recovery.
pub struct SqliteClaimer {
    pool: SqlitePool,
}

impl SqliteClaimer {
    /// Creates a claimer over `pool`, creating the claims table if it's missing.
    pub async fn new(pool: SqlitePool) -> Result<Self, SqliteError> {
        todo!()
    }
}

impl Claimer for SqliteClaimer {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
        todo!()
    }
}

/// A held row in the claims table.
struct SqliteClaimLock {
    pool: SqlitePool,
    key: Key,
    owner: String,
}

impl ClaimLock for SqliteClaimLock {
    fn release(self: Box<Self>) -> Result<(), Error> {
        todo!()
    }
}
