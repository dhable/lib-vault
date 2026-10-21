use crate::SqliteError;
use crate::error::claim_error;
use futures::future::BoxFuture;
use lib_vault::{ClaimLock, Claimer, Error, Key};
use sqlx::sqlite::SqlitePool;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// Options for a [`SqliteClaimer`]. Private fields with builder methods, like
/// `FsStoreOptions` (D66), so options can be added later.
#[derive(Clone, Debug)]
pub struct SqliteClaimerOptions {
    lease: Duration,
}

impl Default for SqliteClaimerOptions {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30),
        }
    }
}

impl SqliteClaimerOptions {
    /// How long this claimer's claims stay valid without a renewal. The claimer renews three
    /// times per lease while it's alive; if its process dies, its claims are free once the
    /// lease runs out. Default: 30 seconds.
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }
}

/// A `Claimer` that records claims in SQLite, so processes sharing the database file exclude
/// each other. Works with any store.
///
/// **Tables.** `vault_claim_owners(owner, expires_at)` has a row per live claimer, and
/// `vault_claims(key, owner, token, acquired_at)` a row per claim. Both are created by
/// [`new`](Self::new).
///
/// **Claiming.** `try_lock` first checks the claimer's in-process set of held keys, then runs
/// one `BEGIN IMMEDIATE` transaction that renews the claimer's lease, deletes the claims of
/// owners whose lease has expired, refuses the key if another owner holds the same key, an
/// ancestor or a descendant (D44), and otherwise inserts the claim's row. It never waits for
/// another claim; it can wait up to the pool's busy timeout for SQLite's write lock.
///
/// **Releasing** is synchronous and never touches the database (D39): it removes the key
/// from the in-process set, so this claimer and its vaults can claim the key again
/// immediately, and queues the row's deletion for a background task. Other processes see the
/// release once that task has run, normally within milliseconds.
///
/// **Leases.** The background task renews the claimer's lease every third of the lease
/// duration. If the process dies, its claims stop counting once the lease runs out, and the
/// next `try_lock` anywhere deletes them. A claimer that can't renew in time (its runtime is
/// blocked for longer than the lease) can lose its claims to another process, so choose a
/// lease well above any expected stall.
///
/// **Shutting down.** Keep a clone of the claimer, and after `Vault::shutdown` call
/// [`shutdown`](Self::shutdown) before closing the pool: it waits for the queued deletions and
/// removes the claimer's rows, so other processes don't wait for the lease. When the claimer
/// and all its claims are dropped without it, the background task does the same on its own,
/// but nothing waits for it.
///
/// Clones share the claimer: the same held keys, rows and lease, like `MemoryClaimer`'s
/// clones. Requires a tokio runtime: `new` spawns the background task.
#[derive(Clone)]
pub struct SqliteClaimer {
    inner: Arc<Inner>,
}

struct Inner {
    // Unique per claimer, across processes.
    owner: String,
    pool: SqlitePool,
    lease: Duration,
    // Keys this claimer holds. Authoritative for its own claims: its rows in the table may
    // lag behind (queued deletions), so the database check skips them.
    held: Mutex<HashSet<Key>>,
    // Tells this claim's row apart from a later claim on the same key by the same owner, so
    // a queued deletion can't remove the newer row.
    next_token: AtomicI64,
    shut_down: AtomicBool,
    queue: mpsc::UnboundedSender<Message>,
}

enum Message {
    Release(Release),
    Shutdown(oneshot::Sender<Result<(), sqlx::Error>>),
}

struct Release {
    key: Key,
    token: i64,
}

impl SqliteClaimer {
    /// Creates a claimer over `pool` with the default options, creating the claim tables if
    /// they're missing.
    pub async fn new(pool: SqlitePool) -> Result<Self, SqliteError> {
        Self::with_options(pool, SqliteClaimerOptions::default()).await
    }

    /// Creates a claimer over `pool` with `options`.
    pub async fn with_options(
        pool: SqlitePool,
        options: SqliteClaimerOptions,
    ) -> Result<Self, SqliteError> {
        let owner = new_owner_id();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS vault_claim_owners (
                owner TEXT PRIMARY KEY,
                expires_at INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS vault_claims (
                key TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                token INTEGER NOT NULL,
                acquired_at INTEGER NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS vault_claims_owner ON vault_claims (owner)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO vault_claim_owners (owner, expires_at) VALUES (?, ?)")
            .bind(&owner)
            .bind(now_ms() + millis(options.lease))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let (queue, queued) = mpsc::unbounded_channel();
        tokio::spawn(background(
            pool.clone(),
            owner.clone(),
            options.lease,
            queued,
        ));
        Ok(Self {
            inner: Arc::new(Inner {
                owner,
                pool,
                lease: options.lease,
                held: Mutex::new(HashSet::new()),
                next_token: AtomicI64::new(0),
                shut_down: AtomicBool::new(false),
                queue,
            }),
        })
    }

    /// Finishes the queued row deletions, deletes all of this claimer's rows and stops
    /// renewing its lease. Call it after `Vault::shutdown` (so no claims are held) and before
    /// closing the pool. Afterwards `try_lock` fails with [`SqliteError::ClaimerShutDown`].
    /// Calling it again does nothing.
    pub async fn shutdown(&self) -> Result<(), SqliteError> {
        if self.inner.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let (done, finished) = oneshot::channel();
        if self.inner.queue.send(Message::Shutdown(done)).is_err() {
            return Ok(()); // the background task is already gone
        }
        match finished.await {
            Ok(result) => Ok(result?),
            Err(_) => Ok(()), // the task stopped without answering: nothing left to renew
        }
    }
}

impl Claimer for SqliteClaimer {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
        let key = key.clone();
        Box::pin(async move {
            if self.inner.shut_down.load(Ordering::SeqCst) {
                return Err(claim_error(SqliteError::ClaimerShutDown));
            }
            let mut held = Inner::reserve(&self.inner, &key)?;
            self.inner.insert_row(&mut held).await?;
            Ok(Box::new(SqliteClaimLock { _held: held }) as Box<dyn ClaimLock>)
        })
    }
}

impl Inner {
    // Claims `key` in this process, or refuses it if this claimer already holds an
    // overlapping key. The returned guard gives the key back when dropped.
    fn reserve(this: &Arc<Self>, key: &Key) -> Result<HeldKey, Error> {
        let mut held = this.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held
            .iter()
            .any(|h| h.starts_with(key) || key.starts_with(h))
        {
            return Err(Error::AlreadyClaimed { key: key.clone() });
        }
        held.insert(key.clone());
        Ok(HeldKey {
            inner: this.clone(),
            key: key.clone(),
            token: this.next_token.fetch_add(1, Ordering::Relaxed),
            row_written: false,
        })
    }

    // Inserts the claim's row, or refuses the key if another live owner holds an overlapping
    // one. One transaction, so the check and the insert can't interleave with another
    // process's.
    async fn insert_row(&self, held: &mut HeldKey) -> Result<(), Error> {
        let key = held.key.as_str();
        let now = now_ms();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(claim_error)?;

        // Renew our lease first, so the cleanup below can't count our own rows as stale.
        sqlx::query(
            "INSERT INTO vault_claim_owners (owner, expires_at) VALUES (?, ?)
             ON CONFLICT (owner) DO UPDATE SET expires_at = excluded.expires_at",
        )
        .bind(&self.owner)
        .bind(now + millis(self.lease))
        .execute(&mut *tx)
        .await
        .map_err(claim_error)?;

        // Owners whose lease ran out (crashed processes) no longer hold anything.
        sqlx::query("DELETE FROM vault_claim_owners WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(claim_error)?;
        sqlx::query(
            "DELETE FROM vault_claims WHERE owner NOT IN (SELECT owner FROM vault_claim_owners)",
        )
        .execute(&mut *tx)
        .await
        .map_err(claim_error)?;

        // Our own rows are skipped: `held` already decided between our claims.
        let conflict = sqlx::query(
            "SELECT 1 FROM vault_claims
             WHERE owner <> ?
               AND (key = ?
                    OR substr(?, 1, length(key) + 1) = key || '/'
                    OR substr(key, 1, length(?) + 1) = ? || '/')
             LIMIT 1",
        )
        .bind(&self.owner)
        .bind(key)
        .bind(key)
        .bind(key)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(claim_error)?;
        if conflict.is_some() {
            return Err(Error::AlreadyClaimed {
                key: held.key.clone(),
            });
        }

        // `OR REPLACE` only ever replaces one of our own rows whose deletion is still queued.
        sqlx::query(
            "INSERT OR REPLACE INTO vault_claims (key, owner, token, acquired_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(key)
        .bind(&self.owner)
        .bind(held.token)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(claim_error)?;

        // From here the row may exist even if this future is dropped, so the guard must
        // queue its deletion.
        held.row_written = true;
        tx.commit().await.map_err(claim_error)
    }
}

/// A key held by a claimer. Dropping it releases the key: at once in this process, and in
/// the database once the background task deletes the row. Also releases a claim whose
/// `try_lock` future was dropped part-way (D44).
struct HeldKey {
    inner: Arc<Inner>,
    key: Key,
    token: i64,
    row_written: bool,
}

impl Drop for HeldKey {
    fn drop(&mut self) {
        self.inner
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        if !self.row_written {
            return;
        }
        let release = Release {
            key: self.key.clone(),
            token: self.token,
        };
        if self.inner.queue.send(Message::Release(release)).is_err() {
            tracing::warn!(
                key = %self.key,
                "sqlite claimer's background task has stopped; the claim's row stays until its lease expires"
            );
        }
    }
}

/// A claim held by [`SqliteClaimer`].
struct SqliteClaimLock {
    _held: HeldKey,
}

impl ClaimLock for SqliteClaimLock {
    /// Releases the key in this process and queues the row's deletion; never blocks.
    fn release(self: Box<Self>) -> Result<(), Error> {
        drop(self);
        Ok(())
    }
}

// Deletes released claims' rows and renews the lease until `shutdown`, or until the claimer
// and all its claims are dropped (the channel closes); then deletes everything the claimer
// still owns. Messages are handled in order, so `shutdown` follows every queued deletion.
async fn background(
    pool: SqlitePool,
    owner: String,
    lease: Duration,
    mut queue: mpsc::UnboundedReceiver<Message>,
) {
    let mut renew = tokio::time::interval((lease / 3).max(Duration::from_millis(1)));
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renew.tick().await; // the first tick completes immediately
    loop {
        tokio::select! {
            message = queue.recv() => match message {
                Some(Message::Release(release)) => {
                    if let Err(err) = delete_row(&pool, &owner, &release).await {
                        tracing::warn!(key = %release.key, error = %err, "couldn't delete a released claim's row; it stays until the lease expires");
                    }
                }
                Some(Message::Shutdown(done)) => {
                    let _ = done.send(retire(&pool, &owner).await);
                    return;
                }
                None => break,
            },
            _ = renew.tick() => {
                if let Err(err) = renew_lease(&pool, &owner, lease).await {
                    tracing::warn!(error = %err, "couldn't renew the sqlite claimer's lease");
                }
            }
        }
    }
    if let Err(err) = retire(&pool, &owner).await {
        tracing::warn!(error = %err, "couldn't delete a dropped sqlite claimer's rows; they stay until its lease expires");
    }
}

async fn delete_row(pool: &SqlitePool, owner: &str, release: &Release) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM vault_claims WHERE key = ? AND owner = ? AND token = ?")
        .bind(release.key.as_str())
        .bind(owner)
        .bind(release.token)
        .execute(pool)
        .await?;
    Ok(())
}

async fn renew_lease(pool: &SqlitePool, owner: &str, lease: Duration) -> Result<(), sqlx::Error> {
    let renewed = sqlx::query("UPDATE vault_claim_owners SET expires_at = ? WHERE owner = ?")
        .bind(now_ms() + millis(lease))
        .bind(owner)
        .execute(pool)
        .await?;
    if renewed.rows_affected() == 0 {
        // Our lease ran out and another process deleted our rows: claims we still hold in
        // this process are no longer exclusive. Re-register so new claims count again.
        tracing::warn!(
            owner,
            "sqlite claimer's lease expired before renewal; its earlier claims may have been taken over"
        );
        sqlx::query(
            "INSERT INTO vault_claim_owners (owner, expires_at) VALUES (?, ?)
             ON CONFLICT (owner) DO UPDATE SET expires_at = excluded.expires_at",
        )
        .bind(owner)
        .bind(now_ms() + millis(lease))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn retire(pool: &SqlitePool, owner: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM vault_claims WHERE owner = ?")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM vault_claim_owners WHERE owner = ?")
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

// Unique across processes: pid, start time and a per-process counter.
fn new_owner_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{n}", std::process::id())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or_default()
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
