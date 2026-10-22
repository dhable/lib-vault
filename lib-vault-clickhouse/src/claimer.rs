use crate::ClickhouseError;
use crate::error::claim_error;
use crate::protocol::{
    KeeperConflict, TOKEN_SEPARATOR, keeper_conflict, lock_plan, new_owner_id, token_owner,
    validate_root_path,
};
use clickhouse::Client;
use futures::future::BoxFuture;
use lib_vault::{ClaimLock, Claimer, Error, Key};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// Options for a [`ClickhouseClaimer`]. Private fields with builder methods, like
/// `FsStoreOptions` (D66), so options can be added later.
#[derive(Clone, Debug)]
pub struct ClickhouseClaimerOptions {
    lease: Duration,
    root_path: String,
    max_attempts: u32,
}

impl Default for ClickhouseClaimerOptions {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(30),
            root_path: "/lib_vault".to_owned(),
            max_attempts: 25,
        }
    }
}

impl ClickhouseClaimerOptions {
    /// How long this claimer's claims stay valid without a renewal. The claimer renews three
    /// times per lease while it's alive; if its process dies, its claims are free once the
    /// lease runs out. Measured with the ClickHouse server's clock. Default: 30 seconds.
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// The Keeper path the claim tables live under (the server adds its
    /// `keeper_map_path_prefix`). Claimers exclude each other when they use the same path on
    /// servers sharing a Keeper. `/`-separated components of `[A-Za-z0-9_-]`. Default:
    /// `/lib_vault`.
    pub fn root_path(mut self, root_path: impl Into<String>) -> Self {
        self.root_path = root_path.into();
        self
    }

    /// How many times one row update may be retried when concurrent claims keep changing
    /// the row first, before `try_lock` gives up with [`ClickhouseError::Contended`]. Each
    /// attempt is a version-checked write, not a wait for another claim; attempts are
    /// spaced by a short random backoff. Default: 25.
    pub fn max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }
}

/// A `Claimer` that records claims in Keeper through two `KeeperMap` tables, so processes and
/// ClickHouse servers sharing a Keeper exclude each other. Works with any store.
///
/// **Tables**, created by [`new`](Self::new) in the client's database:
/// `vault_claims (key, excl, holders)` with a row per locked key component, and
/// `vault_claim_owners (owner, expires_at)` with a row per live claimer. Every write uses
/// `keeper_map_strict_mode`, so it's a single Keeper `multi` that fails if a row it relies on
/// changed. Inserts are synchronous (`async_insert = 0`), so one claimer's conflict can't
/// fail another's batched insert.
///
/// **Claiming** `a/b/c` (D44, D67-style): add the claim's token to the shared `holders` of
/// `a` and `a/b` (only while the row has no exclusive holder), then make it the exclusive
/// holder (`excl`) of `a/b/c` (only while the row has neither). Each step creates the row if
/// it's missing, or runs a conditional `ALTER UPDATE`, then reads the row back. Keeper reads
/// are only sequentially consistent, but every success depends on a version-checked write,
/// so a stale read can only cause a refusal, never a wrong success. `try_lock` never waits
/// for another claim; it retries a write that lost to a concurrent change of the same row,
/// up to [`ClickhouseClaimerOptions::max_attempts`].
///
/// **Releasing** is synchronous and never touches the server (D39): the claim's rows are
/// updated by a background task, and `try_lock` on this claimer first waits for queued
/// releases, so this claimer can hand a released key out again at once. Other claimers see
/// the release once the task has run.
///
/// **Leases.** The background task renews this claimer's `expires_at` every third of the
/// lease. A claim held by an owner whose lease has run out is freed by the next conflicting
/// `try_lock`: it deletes the owner's row with a version-checked delete (which fails if the
/// lease was renewed in between) and removes the owner's tokens. A claimer that can't renew
/// in time (its runtime blocked for longer than the lease) can lose its claims, so choose a
/// lease well above any expected stall.
///
/// **Shutting down.** Keep a clone of the claimer and, after `Vault::shutdown`, call
/// [`shutdown`](Self::shutdown): it finishes the queued releases and removes this claimer's
/// rows. Dropping the claimer and all its claims does the same in the background, unawaited.
///
/// Clones share the claimer. Requires a tokio runtime: `new` spawns the background task.
#[derive(Clone)]
pub struct ClickhouseClaimer {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    owner: String,
    max_attempts: u32,
    next_token: AtomicU64,
    shut_down: AtomicBool,
    queue: mpsc::UnboundedSender<Message>,
}

enum Message {
    Release { key: Key, token: String },
    Flush(oneshot::Sender<()>),
    Shutdown(oneshot::Sender<Result<(), clickhouse::error::Error>>),
}

// One claim row, as read back after a write.
#[derive(clickhouse::Row, serde::Deserialize)]
struct ClaimRow {
    excl: String,
    holders: Vec<String>,
}

impl ClickhouseClaimer {
    /// Creates a claimer over `client` with the default options, creating the claim tables
    /// if they're missing and registering this claimer as an owner.
    pub async fn new(client: Client) -> Result<Self, ClickhouseError> {
        Self::with_options(client, ClickhouseClaimerOptions::default()).await
    }

    /// Creates a claimer over `client` with `options`.
    pub async fn with_options(
        client: Client,
        options: ClickhouseClaimerOptions,
    ) -> Result<Self, ClickhouseError> {
        validate_root_path(&options.root_path).map_err(ClickhouseError::InvalidOption)?;
        if options.lease < Duration::from_millis(3) {
            return Err(ClickhouseError::InvalidOption(format!(
                "lease {:?} is too short",
                options.lease
            )));
        }
        let client = client
            .with_setting("keeper_map_strict_mode", "1")
            .with_setting("async_insert", "0");

        client
            .query(
                "CREATE TABLE IF NOT EXISTS vault_claim_owners (owner String, expires_at Int64)
                 ENGINE = KeeperMap(?) PRIMARY KEY owner",
            )
            .bind(format!("{}/owners", options.root_path))
            .execute()
            .await?;
        client
            .query(
                "CREATE TABLE IF NOT EXISTS vault_claims
                    (key String, excl String, holders Array(String))
                 ENGINE = KeeperMap(?) PRIMARY KEY key",
            )
            .bind(format!("{}/claims", options.root_path))
            .execute()
            .await?;

        let owner = new_owner_id();
        register_owner(&client, &owner, options.lease).await?;

        let (queue, queued) = mpsc::unbounded_channel();
        tokio::spawn(background(
            client.clone(),
            owner.clone(),
            options.lease,
            options.max_attempts,
            queued,
        ));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                owner,
                max_attempts: options.max_attempts,
                next_token: AtomicU64::new(0),
                shut_down: AtomicBool::new(false),
                queue,
            }),
        })
    }

    /// Finishes the queued releases, removes all of this claimer's rows and stops renewing
    /// its lease. Call it after `Vault::shutdown` (so no claims are held). Afterwards
    /// `try_lock` fails with [`ClickhouseError::ClaimerShutDown`]. Calling it again does
    /// nothing.
    pub async fn shutdown(&self) -> Result<(), ClickhouseError> {
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

impl Claimer for ClickhouseClaimer {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
        let key = key.clone();
        Box::pin(async move {
            let inner = &self.inner;
            if inner.shut_down.load(Ordering::SeqCst) {
                return Err(claim_error(ClickhouseError::ClaimerShutDown));
            }
            inner.flush().await;

            let mut held = HeldClaim {
                inner: inner.clone(),
                token: format!(
                    "{}{TOKEN_SEPARATOR}{}",
                    inner.owner,
                    inner.next_token.fetch_add(1, Ordering::Relaxed)
                ),
                key,
                written: false,
            };
            let (ancestors, target) = lock_plan(&held.key);
            // From the first write on, a failure or a dropped future must release whatever
            // landed; releasing a row the token never reached is a no-op.
            held.written = true;
            for path in &ancestors {
                inner.acquire_shared(&held.key, path, &held.token).await?;
            }
            inner
                .acquire_exclusive(&held.key, &target, &held.token)
                .await?;
            Ok(Box::new(ClickhouseClaimLock { _held: held }) as Box<dyn ClaimLock>)
        })
    }
}

impl Inner {
    // Waits until every release queued before this call has been applied.
    async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        if self.queue.send(Message::Flush(done)).is_ok() {
            let _ = flushed.await;
        }
    }

    // Adds `token` to the shared holders of `path`, unless the row has an exclusive holder.
    async fn acquire_shared(&self, key: &Key, path: &str, token: &str) -> Result<(), Error> {
        for attempt in 0..self.max_attempts {
            backoff(attempt).await;
            if self.try_insert(path, "", &[token]).await? {
                return Ok(());
            }
            let updated = self
                .client
                .query(
                    "ALTER TABLE vault_claims UPDATE holders = arrayPushBack(holders, ?)
                     WHERE key = ? AND excl = '' AND NOT has(holders, ?)",
                )
                .bind(token)
                .bind(path)
                .bind(token)
                .execute()
                .await;
            if !self.applied(updated)? {
                continue;
            }
            match self.read_row(path).await? {
                None => continue, // removed in between: create it again
                Some(row) if row.holders.iter().any(|h| h == token) => return Ok(()),
                Some(row) if !row.excl.is_empty() => {
                    if self.reap(path, std::slice::from_ref(&row.excl)).await? {
                        continue;
                    }
                    return Err(Error::AlreadyClaimed { key: key.clone() });
                }
                Some(_) => continue, // the update read a stale row and changed nothing
            }
        }
        Err(self.contended(key))
    }

    // Makes `token` the exclusive holder of `path`, unless the row has any holder.
    async fn acquire_exclusive(&self, key: &Key, path: &str, token: &str) -> Result<(), Error> {
        for attempt in 0..self.max_attempts {
            backoff(attempt).await;
            if self.try_insert(path, token, &[]).await? {
                return Ok(());
            }
            let updated = self
                .client
                .query(
                    "ALTER TABLE vault_claims UPDATE excl = ?
                     WHERE key = ? AND excl = '' AND empty(holders)",
                )
                .bind(token)
                .bind(path)
                .execute()
                .await;
            if !self.applied(updated)? {
                continue;
            }
            match self.read_row(path).await? {
                None => continue,
                Some(row) if row.excl == token => return Ok(()),
                Some(row) => {
                    let mut holders = row.holders;
                    if !row.excl.is_empty() {
                        holders.push(row.excl);
                    }
                    if holders.is_empty() {
                        continue; // stale read: the row is free now
                    }
                    if self.reap(path, &holders).await? {
                        continue;
                    }
                    return Err(Error::AlreadyClaimed { key: key.clone() });
                }
            }
        }
        Err(self.contended(key))
    }

    // Creates the row for `path`. `false` if it already exists.
    async fn try_insert(&self, path: &str, excl: &str, holders: &[&str]) -> Result<bool, Error> {
        let inserted = self
            .client
            .query("INSERT INTO vault_claims (key, excl, holders) VALUES (?, ?, ?)")
            .bind(path)
            .bind(excl)
            .bind(holders)
            .execute()
            .await;
        match inserted {
            Ok(()) => Ok(true),
            Err(err) if keeper_conflict(&err) == Some(KeeperConflict::NodeExists) => Ok(false),
            Err(err) => Err(claim_error(err)),
        }
    }

    // `true` if a version-checked write went through, `false` if it lost to a concurrent
    // change and should be retried.
    fn applied(&self, result: Result<(), clickhouse::error::Error>) -> Result<bool, Error> {
        match result {
            Ok(()) => Ok(true),
            Err(err) if keeper_conflict(&err) == Some(KeeperConflict::Lost) => Ok(false),
            Err(err) => Err(claim_error(err)),
        }
    }

    async fn read_row(&self, path: &str) -> Result<Option<ClaimRow>, Error> {
        self.client
            .query("SELECT ?fields FROM vault_claims WHERE key = ?")
            .bind(path)
            .fetch_optional::<ClaimRow>()
            .await
            .map_err(claim_error)
    }

    // Frees `path` from the tokens of owners whose lease has run out (P66-style). `true` if
    // any owner among `tokens` turned out to be dead, so the caller should try again.
    async fn reap(&self, path: &str, tokens: &[String]) -> Result<bool, Error> {
        let mut owners: Vec<&str> = tokens.iter().map(|t| token_owner(t)).collect();
        owners.sort_unstable();
        owners.dedup();

        let mut dead = Vec::new();
        for owner in owners {
            if owner == self.owner {
                continue; // our own live claim: releases were flushed before this try_lock
            }
            // Version-checked: fails (or matches nothing) if the lease was renewed after the
            // scan, so a live owner is never declared dead.
            let deleted = self
                .client
                .query(
                    "DELETE FROM vault_claim_owners
                     WHERE owner = ? AND expires_at <= toUnixTimestamp64Milli(now64(3))",
                )
                .bind(owner)
                .execute()
                .await;
            self.applied(deleted)?;
            let alive: u64 = self
                .client
                .query("SELECT count() FROM vault_claim_owners WHERE owner = ?")
                .bind(owner)
                .fetch_one()
                .await
                .map_err(claim_error)?;
            if alive == 0 {
                dead.push(owner.to_owned());
            }
        }
        if dead.is_empty() {
            return Ok(false);
        }

        let removed = self
            .client
            .query(
                "ALTER TABLE vault_claims UPDATE
                    holders = arrayFilter(h -> NOT has(?, splitByChar('#', h)[1]), holders),
                    excl = if(has(?, splitByChar('#', excl)[1]), '', excl)
                 WHERE key = ?",
            )
            .bind(&dead)
            .bind(&dead)
            .bind(path)
            .execute()
            .await;
        self.applied(removed)?;
        Ok(true)
    }

    fn contended(&self, key: &Key) -> Error {
        claim_error(ClickhouseError::Contended {
            key: key.clone(),
            attempts: self.max_attempts,
        })
    }
}

/// A claim's token, from its first write on. Dropping it queues the release of every row
/// the token may have reached: for a held claim, and for a `try_lock` that failed or was
/// dropped part-way (D44).
struct HeldClaim {
    inner: Arc<Inner>,
    key: Key,
    token: String,
    written: bool,
}

impl Drop for HeldClaim {
    fn drop(&mut self) {
        if !self.written {
            return;
        }
        let release = Message::Release {
            key: self.key.clone(),
            token: std::mem::take(&mut self.token),
        };
        if self.inner.queue.send(release).is_err() {
            tracing::warn!(
                key = %self.key,
                "clickhouse claimer's background task has stopped; the claim stays until its lease expires"
            );
        }
    }
}

/// A claim held by [`ClickhouseClaimer`].
struct ClickhouseClaimLock {
    _held: HeldClaim,
}

impl ClaimLock for ClickhouseClaimLock {
    /// Queues the claim's release and returns; never blocks.
    fn release(self: Box<Self>) -> Result<(), Error> {
        drop(self);
        Ok(())
    }
}

async fn register_owner(
    client: &Client,
    owner: &str,
    lease: Duration,
) -> Result<(), clickhouse::error::Error> {
    client
        .query(
            "INSERT INTO vault_claim_owners
             SELECT ?, toUnixTimestamp64Milli(now64(3)) + ?",
        )
        .bind(owner)
        .bind(millis(lease))
        .execute()
        .await
}

// Applies releases in order, answers flushes, and renews the lease until `shutdown` or until
// the claimer and all its claims are dropped (the channel closes); then removes everything
// the claimer still owns.
async fn background(
    client: Client,
    owner: String,
    lease: Duration,
    max_attempts: u32,
    mut queue: mpsc::UnboundedReceiver<Message>,
) {
    let mut renew = tokio::time::interval(lease / 3);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renew.tick().await; // the first tick completes immediately
    // Release retries outlast `try_lock`'s: a token left behind blocks its key until the
    // claimer retires or its lease runs out.
    let release_attempts = max_attempts.saturating_mul(5);
    loop {
        tokio::select! {
            message = queue.recv() => match message {
                Some(Message::Release { key, token }) => {
                    if let Err(err) = release(&client, &key, &token, release_attempts).await {
                        tracing::warn!(%key, error = %err, "couldn't release a clickhouse claim; it stays until the claimer retires or its lease expires");
                    }
                }
                Some(Message::Flush(done)) => {
                    let _ = done.send(());
                }
                Some(Message::Shutdown(done)) => {
                    let _ = done.send(retire(&client, &owner, release_attempts).await);
                    return;
                }
                None => break,
            },
            _ = renew.tick() => {
                if let Err(err) = renew_lease(&client, &owner, lease).await {
                    tracing::warn!(error = %err, "couldn't renew the clickhouse claimer's lease");
                }
            }
        }
    }
    if let Err(err) = retire(&client, &owner, release_attempts).await {
        tracing::warn!(error = %err, "couldn't remove a dropped clickhouse claimer's rows; they stay until its lease expires");
    }
}

// Removes `token` from the rows of `key`'s lock plan: the exclusive hold first, then the
// shared holds bottom-up (D29), then deletes rows nobody holds any more.
async fn release(
    client: &Client,
    key: &Key,
    token: &str,
    attempts: u32,
) -> Result<(), clickhouse::error::Error> {
    let (ancestors, target) = lock_plan(key);
    retry_version_conflicts(attempts, || {
        client
            .query("ALTER TABLE vault_claims UPDATE excl = '' WHERE key = ? AND excl = ?")
            .bind(&target)
            .bind(token)
            .execute()
    })
    .await?;
    for path in ancestors.iter().rev() {
        retry_version_conflicts(attempts, || {
            client
                .query(
                    "ALTER TABLE vault_claims UPDATE holders = arrayFilter(h -> h != ?, holders)
                     WHERE key = ? AND has(holders, ?)",
                )
                .bind(token)
                .bind(path)
                .bind(token)
                .execute()
        })
        .await?;
    }
    for path in std::iter::once(&target).chain(ancestors.iter().rev()) {
        delete_if_free(client, path).await?;
    }
    Ok(())
}

// Deletes the row for `path` if nobody holds it. Losing to a concurrent change is fine: that
// change came from a claim that now holds the row.
async fn delete_if_free(client: &Client, path: &str) -> Result<(), clickhouse::error::Error> {
    let deleted = client
        .query("DELETE FROM vault_claims WHERE key = ? AND excl = '' AND empty(holders)")
        .bind(path)
        .execute()
        .await;
    match deleted {
        Err(err) if keeper_conflict(&err) != Some(KeeperConflict::Lost) => Err(err),
        _ => Ok(()),
    }
}

async fn renew_lease(
    client: &Client,
    owner: &str,
    lease: Duration,
) -> Result<(), clickhouse::error::Error> {
    let renewed = client
        .query(
            "ALTER TABLE vault_claim_owners
             UPDATE expires_at = toUnixTimestamp64Milli(now64(3)) + ? WHERE owner = ?",
        )
        .bind(millis(lease))
        .bind(owner)
        .execute()
        .await;
    match renewed {
        // Lost to a concurrent change: either another claimer just reaped us (checked
        // below) or the next tick will renew.
        Err(err) if keeper_conflict(&err) == Some(KeeperConflict::Lost) => {}
        Err(err) => return Err(err),
        Ok(()) => {}
    }
    let registered: u64 = client
        .query("SELECT count() FROM vault_claim_owners WHERE owner = ?")
        .bind(owner)
        .fetch_one()
        .await?;
    if registered == 0 {
        // Our lease ran out and another claimer reaped us: claims we still hold may have
        // been taken over. Re-register so new claims count again.
        tracing::warn!(
            owner,
            "clickhouse claimer's lease expired before renewal; its earlier claims may have been taken over"
        );
        match register_owner(client, owner, lease).await {
            Err(err) if keeper_conflict(&err) != Some(KeeperConflict::NodeExists) => {
                return Err(err);
            }
            _ => {}
        }
    }
    Ok(())
}

// Removes every token of `owner`, then the owner row, then rows nobody holds.
async fn retire(
    client: &Client,
    owner: &str,
    attempts: u32,
) -> Result<(), clickhouse::error::Error> {
    let prefix = format!("{owner}{TOKEN_SEPARATOR}");
    retry_version_conflicts(attempts, || {
        client
            .query(
                "ALTER TABLE vault_claims UPDATE
                    holders = arrayFilter(h -> NOT startsWith(h, ?), holders),
                    excl = if(startsWith(excl, ?), '', excl)
                 WHERE startsWith(excl, ?) OR arrayExists(h -> startsWith(h, ?), holders)",
            )
            .bind(&prefix)
            .bind(&prefix)
            .bind(&prefix)
            .bind(&prefix)
            .execute()
    })
    .await?;
    client
        .query("DELETE FROM vault_claim_owners WHERE owner = ?")
        .bind(owner)
        .execute()
        .await?;
    let gc = client
        .query("DELETE FROM vault_claims WHERE excl = '' AND empty(holders)")
        .execute()
        .await;
    match gc {
        Err(err) if keeper_conflict(&err) != Some(KeeperConflict::Lost) => Err(err),
        _ => Ok(()),
    }
}

// Runs a version-checked write until it isn't refused for a concurrent change, backing off
// a little between attempts.
async fn retry_version_conflicts<F, Fut>(
    attempts: u32,
    mut write: F,
) -> Result<(), clickhouse::error::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), clickhouse::error::Error>>,
{
    let mut attempt = 0;
    loop {
        match write().await {
            Err(err)
                if keeper_conflict(&err) == Some(KeeperConflict::Lost)
                    && attempt + 1 < attempts =>
            {
                attempt += 1;
                backoff(attempt).await;
            }
            result => return result,
        }
    }
}

// Spaces out retries of a write that lost to a concurrent change: nothing before the first
// attempt, then a random 1..=4·attempt ms, so contending claims don't retry in lockstep.
async fn backoff(attempt: u32) {
    if attempt == 0 {
        return;
    }
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or_default();
    let ms = 1 + jitter % (4 * u64::from(attempt));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}
