//! Claim exclusion through `SqliteClaimer` (D44, D78): within a process, between claimers
//! sharing a database (each one stands in for another process), leases for crashed owners,
//! and a real second process.

use crate::app::{AppDispatcher, Doc};
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::io_driver::IoDriver;
use lib_vault::{Error, Key, Vault};
use lib_vault_memory::MemoryStore;
use lib_vault_sqlite::{SqliteClaimer, SqliteClaimerOptions, SqliteError, SqliteStore};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

async fn refused<D: IoDriver>(v: &Vault<D>, k: &str) -> bool {
    match v.claim(Key::new(k).unwrap()).await {
        Err(Error::AlreadyClaimed { key: refused }) => {
            assert_eq!(refused, Key::new(k).unwrap());
            true
        }
        Err(other) => panic!("unexpected error for {k}: {other:?}"),
        Ok(_) => false,
    }
}

// Another claimer sees a release once the background task has deleted the row: retry for
// a while instead of expecting it at once.
async fn eventually_claimable<D: IoDriver>(v: &Vault<D>, k: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !refused(v, k).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_claims_conflict_in_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    let held = v.claim(Key::new("a/b").unwrap()).await.unwrap();
    assert!(refused(&v, "a/b").await);
    assert!(refused(&v, "a/b/c").await); // descendant
    assert!(refused(&v, "a").await); // ancestor
    assert!(!refused(&v, "a/c").await); // sibling
    assert!(!refused(&v, "a/bc").await); // shares only a string prefix
    drop(held);
    assert!(!refused(&v, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_and_drop_free_the_key_immediately_in_process() {
    // D39: `Ok` from release means this claimer can hand the key out again right away, even
    // though the row's deletion is still queued.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    for _ in 0..20 {
        let c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.release().unwrap();
        let c = v.claim(Key::new("a/b").unwrap()).await.unwrap();
        drop(c);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn claimers_sharing_a_database_exclude_each_other() {
    // Two pools and two claimers on one file: the same as two processes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v1 = Vault::new(store, claimer);

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v2 = Vault::new(store, claimer);

    let held = v1.claim(Key::new("a/b").unwrap()).await.unwrap();
    assert!(refused(&v2, "a/b").await);
    assert!(refused(&v2, "a/b/c").await);
    assert!(refused(&v2, "a").await);
    assert!(!refused(&v2, "a/c").await);
    assert!(!refused(&v2, "a/bc").await);

    // The other claimer sees the release once the row is deleted.
    held.release().unwrap();
    assert!(eventually_claimable(&v2, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_are_rows_in_the_claims_table() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);

    let held = v.claim(Key::new("a/b").unwrap()).await.unwrap();
    let rows = sqlx::query(
        "SELECT c.key, o.expires_at FROM vault_claims c JOIN vault_claim_owners o ON o.owner = c.owner",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("key"), "a/b");
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        rows[0].get::<i64, _>("expires_at") > now_ms,
        "the lease is live"
    );

    held.release().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let claims: i64 = sqlx::query("SELECT count(*) AS n FROM vault_claims")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        if claims == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the released row was never deleted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_claim_holds_nothing() {
    // D47: a failed `try_lock` leaves no row and no in-process hold behind.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v1 = Vault::new(store, claimer);

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool2 = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool2.clone(), AppDispatcher)
        .await
        .unwrap();
    let claimer = SqliteClaimer::new(pool2).await.unwrap();
    let v2 = Vault::new(store, claimer);

    let held = v1.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(refused(&v2, "a/b").await);
    let rows: Vec<String> = sqlx::query("SELECT key FROM vault_claims")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get("key"))
        .collect();
    assert_eq!(rows, ["a"]);

    drop(held);
    assert!(eventually_claimable(&v2, "a/b").await);
    assert!(!refused(&v2, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_leases_free_their_claims() {
    // P66: a crashed process can't renew its lease. Once the lease runs out its rows stop
    // counting, and the next `try_lock` deletes them. A live lease still excludes.
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // What a crashed process leaves behind, and a live one's claim.
    sqlx::query(
        "INSERT INTO vault_claim_owners (owner, expires_at) VALUES ('crashed', ?), ('alive', ?)",
    )
    .bind(now_ms - 1_000)
    .bind(now_ms + 60_000)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO vault_claims (key, owner, token, acquired_at)
         VALUES ('a', 'crashed', 0, 0), ('b', 'alive', 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(!refused(&v, "a/x").await);
    assert!(refused(&v, "b").await);
    assert!(refused(&v, "b/x").await);

    let owners: Vec<String> = sqlx::query("SELECT owner FROM vault_claim_owners")
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get("owner"))
        .collect();
    assert!(!owners.contains(&"crashed".to_owned()), "{owners:?}");
    let stale: i64 = sqlx::query("SELECT count(*) AS n FROM vault_claims WHERE owner = 'crashed'")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("n");
    assert_eq!(stale, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_claimer_renews_its_lease() {
    // A short lease, held well past its length: the background task keeps renewing it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::with_options(
        pool,
        SqliteClaimerOptions::default().lease(Duration::from_millis(300)),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store, claimer);

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v2 = Vault::new(store, claimer);

    let _held = v1.claim(Key::new("a").unwrap()).await.unwrap();
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(refused(&v2, "a").await);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_claimer_deletes_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let options = SqliteConnectOptions::new()
        .filename(dir.path().join("vault.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v = Vault::new(store, claimer);

    let held = v.claim(Key::new("a").unwrap()).await.unwrap();
    drop(held);
    drop(v);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = sqlx::query(
            "SELECT (SELECT count(*) FROM vault_claims) AS claims,
                    (SELECT count(*) FROM vault_claim_owners) AS owners",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if row.get::<i64, _>("claims") == 0 && row.get::<i64, _>("owners") == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the claimer's rows were never deleted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_the_claimer_down_removes_its_rows() {
    // After `Vault::shutdown`, `SqliteClaimer::shutdown` finishes the queued deletions and
    // removes the claimer's rows, so other processes can claim at once.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool.clone()).await.unwrap();
    let v1 = Vault::new(store.clone(), claimer.clone());

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool2 = SqlitePool::connect_with(options).await.unwrap();
    let store2 = SqliteStore::new(pool2.clone(), AppDispatcher)
        .await
        .unwrap();
    let claimer2 = SqliteClaimer::new(pool2).await.unwrap();
    let v2 = Vault::new(store2, claimer2);

    let held = v1.claim(Key::new("a").unwrap()).await.unwrap();
    held.release().unwrap();
    v1.shutdown().await;
    claimer.shutdown().await.unwrap();
    claimer.shutdown().await.unwrap(); // a second call does nothing

    let row = sqlx::query(
        "SELECT (SELECT count(*) FROM vault_claims) AS claims,
                (SELECT count(*) FROM vault_claim_owners) AS owners",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("claims"), 0);
    assert_eq!(
        row.get::<i64, _>("owners"),
        1,
        "only the second claimer is left"
    );
    assert!(
        !refused(&v2, "a").await,
        "no waiting for a queued deletion or a lease"
    );

    // A vault still using the shut-down claimer can't claim.
    let v3 = Vault::new(store, claimer);
    let err = v3.claim(Key::new("b").unwrap()).await.err().unwrap();
    let Error::ClaimError(source) = &err else {
        panic!("expected a claim error, got {err:?}");
    };
    assert!(matches!(
        source.downcast_ref::<SqliteError>(),
        Some(SqliteError::ClaimerShutDown)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_claim_holds_nothing() {
    // D44: dropping the `claim` future releases anything it acquired. Cancel it at several
    // points, from before its transaction starts to after it commits.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v1 = Vault::new(store, claimer);

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v2 = Vault::new(store, claimer);

    // A key per round: the other claimer's own release in a round reaches the database
    // only once its row is deleted, which would leak into the next round.
    for micros in [0, 100, 500, 2_000, 10_000, 100_000] {
        let k = format!("k{micros}");
        let _ = tokio::time::timeout(
            Duration::from_micros(micros),
            v1.claim(Key::new(&k).unwrap()),
        )
        .await;
        assert!(!refused(&v1, &k).await, "cancelled after {micros}µs");
        assert!(
            eventually_claimable(&v2, &k).await,
            "cancelled after {micros}µs"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_claims_on_one_key_admit_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");

    let mut vaults = Vec::new();
    for i in 0..4 {
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
        let claimer = SqliteClaimer::new(pool).await.unwrap();
        vaults.push(Vault::new(store, claimer));
        let _ = i;
    }

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let v = vaults[i % vaults.len()].clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                v.claim(Key::new("shared").unwrap()).await
            })
        })
        .collect();
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.unwrap());
    }
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| matches!(e, Error::AlreadyClaimed { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_claimer_works_with_any_store() {
    // Here a byte store: `SqliteClaimer` only needs the database for its own tables.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.db");

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let v1 = Vault::new(MemoryStore::new(), SqliteClaimer::new(pool).await.unwrap());

    let options = SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let v2 = Vault::new(MemoryStore::new(), SqliteClaimer::new(pool).await.unwrap());

    let mut c = v1.claim(Key::new("notes").unwrap()).await.unwrap();
    let mut w = c.open_write("today.md").await.unwrap();
    w.write_all(b"hello").await.unwrap();
    w.close().await.unwrap();
    drop(w);
    let mut buf = Vec::new();
    let mut r = c.open_read("today.md").await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"hello");

    assert!(refused(&v2, "notes/today.md").await);
}

/// Run as a child process by `claims_exclude_other_processes`: holds a claim until killed.
#[test]
#[ignore = "helper for claims_exclude_other_processes"]
fn child_holds_claim() {
    let (Ok(db), Ok(k)) = (std::env::var("LV_CHILD_DB"), std::env::var("LV_CHILD_KEY")) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let options = SqliteConnectOptions::new()
            .filename(&db)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
        let claimer = SqliteClaimer::with_options(
            pool,
            SqliteClaimerOptions::default().lease(Duration::from_millis(500)),
        )
        .await
        .unwrap();
        let vault = Vault::new(store, claimer);
        let mut claim = vault.claim(Key::new(&k).unwrap()).await.unwrap();
        let mut w = claim.open_write(None).await.unwrap();
        w.put(Doc {
            title: "from the child".into(),
            body: String::new(),
        });
        w.close().await.unwrap();
        println!("LV_CHILD_HELD");
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_exclude_other_processes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    let store = SqliteStore::new(pool.clone(), AppDispatcher).await.unwrap();
    let claimer = SqliteClaimer::new(pool).await.unwrap();
    let v = Vault::new(store, claimer);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "claims::child_holds_claim",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LV_CHILD_DB", &path)
        .env("LV_CHILD_KEY", "shared")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let held = tokio::task::spawn_blocking(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("LV_CHILD_HELD"))
    })
    .await
    .unwrap();
    assert!(held, "child process didn't take the claim");

    assert!(refused(&v, "shared").await);
    assert!(refused(&v, "shared/x").await);

    // `kill -9` gives the child no chance to release: its claim stops counting once its
    // 500ms lease runs out, and the value it committed is there.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(eventually_claimable(&v, "shared").await);
    let mut c = v.claim(Key::new("shared").unwrap()).await.unwrap();
    let doc: Doc = c.open_read(None).await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "from the child");
}
