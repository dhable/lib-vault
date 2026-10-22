//! Claim exclusion through `ClickhouseClaimer` (D44): within a claimer, between claimers
//! sharing a Keeper path (each one stands in for another process or server), leases for
//! crashed owners, and a real second process.

use crate::app::{AppDispatcher, Doc};
use crate::fresh_database;
use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::io_driver::IoDriver;
use lib_vault::{Error, Key, Vault};
use lib_vault_clickhouse::{
    ClickhouseClaimer, ClickhouseClaimerOptions, ClickhouseError, ClickhouseStore,
};
use lib_vault_memory::MemoryStore;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

// Another claimer sees a release once the background task has updated the rows: retry for
// a while instead of expecting it at once.
async fn eventually_claimable<D: IoDriver>(v: &Vault<D>, k: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !refused(v, k).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_claims_conflict_in_both_directions() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
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
    // D39: `Ok` from release means this claimer can hand the key out again right away; its
    // next `try_lock` waits for the queued release first.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    for _ in 0..10 {
        let c = v.claim(Key::new("a").unwrap()).await.unwrap();
        c.release().unwrap();
        let c = v.claim(Key::new("a/b").unwrap()).await.unwrap();
        drop(c);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn claimers_sharing_a_keeper_path_exclude_each_other() {
    // Two clients and two claimers on one root path: the same as two processes.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store, claimer);

    let client = crate::server().with_database(&db);
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(store, claimer);

    let held = v1.claim(Key::new("a/b").unwrap()).await.unwrap();
    assert!(refused(&v2, "a/b").await);
    assert!(refused(&v2, "a/b/c").await);
    assert!(refused(&v2, "a").await);
    assert!(!refused(&v2, "a/c").await);
    assert!(!refused(&v2, "a/bc").await);

    // The other claimer sees the release once the rows are updated.
    held.release().unwrap();
    assert!(eventually_claimable(&v2, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_are_rows_in_the_keeper_map_tables() {
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct ClaimRow {
        key: String,
        excl: String,
        holders: Vec<String>,
    }

    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    let engine = client
        .query("SELECT engine FROM system.tables WHERE database = currentDatabase() AND name = 'vault_claims'")
        .fetch_one::<String>()
        .await
        .unwrap();
    assert_eq!(engine, "KeeperMap");

    let held = v.claim(Key::new("a/b").unwrap()).await.unwrap();
    let rows = client
        .query("SELECT ?fields FROM vault_claims ORDER BY key")
        .fetch_all::<ClaimRow>()
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    // `a` is held shared and `a/b` exclusively, by the same claim's token.
    assert_eq!(rows[0].key, "a");
    assert_eq!(rows[0].excl, "");
    assert_eq!(rows[0].holders.len(), 1);
    assert_eq!(rows[1].key, "a/b");
    assert_eq!(rows[1].excl, rows[0].holders[0]);
    assert!(rows[1].holders.is_empty());

    // The token names a registered owner with a live lease.
    let owner = rows[1].excl.split('#').next().unwrap().to_owned();
    let live = client
        .query(
            "SELECT count() FROM vault_claim_owners
             WHERE owner = ? AND expires_at > toUnixTimestamp64Milli(now64(3))",
        )
        .bind(&owner)
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(live, 1);

    // Released rows are removed once nobody holds them.
    held.release().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = client
            .query("SELECT count() FROM vault_claims")
            .fetch_one::<u64>()
            .await
            .unwrap();
        if left == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the released rows were never removed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_claim_holds_nothing() {
    // D47: a failed `try_lock` leaves no hold behind.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store, claimer);

    let client2 = crate::server().with_database(&db);
    let store = ClickhouseStore::new(client2.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client2,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(store, claimer);

    let held = v1.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(refused(&v2, "a/b").await);
    let rows = client
        .query("SELECT key FROM vault_claims ORDER BY key")
        .fetch_all::<String>()
        .await
        .unwrap();
    assert_eq!(rows, ["a"]);

    drop(held);
    assert!(eventually_claimable(&v2, "a/b").await);
    assert!(eventually_claimable(&v2, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_leases_free_their_claims() {
    // A crashed process can't renew its lease. Once the lease runs out, the next conflicting
    // claim deletes the owner (version-checked) and removes its tokens. A live lease still
    // excludes.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    // What a crashed process leaves behind, and a live one's claims.
    client
        .query(
            "INSERT INTO vault_claim_owners VALUES
                ('crashed', toUnixTimestamp64Milli(now64(3)) - 1000),
                ('alive', toUnixTimestamp64Milli(now64(3)) + 60000)",
        )
        .execute()
        .await
        .unwrap();
    client
        .query(
            "INSERT INTO vault_claims VALUES
                ('a', 'crashed#0', []), ('b', '', ['alive#0']), ('b/c', 'alive#0', [])",
        )
        .execute()
        .await
        .unwrap();

    assert!(!refused(&v, "a/x").await);
    assert!(refused(&v, "b").await);
    assert!(refused(&v, "b/c").await);
    assert!(!refused(&v, "b/d").await); // a sibling of the live claim

    let owners = client
        .query("SELECT owner FROM vault_claim_owners")
        .fetch_all::<String>()
        .await
        .unwrap();
    assert!(!owners.contains(&"crashed".to_owned()), "{owners:?}");
    assert!(owners.contains(&"alive".to_owned()), "{owners:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_claimer_renews_its_lease() {
    // A short lease, held well past its length: the background task keeps renewing it, so
    // the other claimer's version-checked delete of the owner never succeeds.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default()
            .root_path(&root)
            .lease(Duration::from_millis(600)),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store, claimer);

    let client = crate::server().with_database(&db);
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(store, claimer);

    let _held = v1.claim(Key::new("a").unwrap()).await.unwrap();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(refused(&v2, "a").await);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_claimer_removes_its_rows() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    let held = v.claim(Key::new("a/b").unwrap()).await.unwrap();
    drop(held);
    drop(v);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let claims = client
            .query("SELECT count() FROM vault_claims")
            .fetch_one::<u64>()
            .await
            .unwrap();
        let owners = client
            .query("SELECT count() FROM vault_claim_owners")
            .fetch_one::<u64>()
            .await
            .unwrap();
        if claims == 0 && owners == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the claimer's rows were never removed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_the_claimer_down_removes_its_rows() {
    // After `Vault::shutdown`, `ClickhouseClaimer::shutdown` finishes the queued releases and
    // removes the claimer's rows, so other claimers can claim at once.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store.clone(), claimer.clone());

    let client2 = crate::server().with_database(&db);
    let store2 = ClickhouseStore::new(client2.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer2 = ClickhouseClaimer::with_options(
        client2,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(store2, claimer2);

    let held = v1.claim(Key::new("a/b").unwrap()).await.unwrap();
    held.release().unwrap();
    v1.shutdown().await;
    claimer.shutdown().await.unwrap();
    claimer.shutdown().await.unwrap(); // a second call does nothing

    let claims = client
        .query("SELECT count() FROM vault_claims")
        .fetch_one::<u64>()
        .await
        .unwrap();
    let owners = client
        .query("SELECT count() FROM vault_claim_owners")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(claims, 0);
    assert_eq!(owners, 1, "only the second claimer is left");
    assert!(
        !refused(&v2, "a").await,
        "no waiting for a queued release or a lease"
    );

    // A vault still using the shut-down claimer can't claim.
    let v3 = Vault::new(store, claimer);
    let err = v3.claim(Key::new("b").unwrap()).await.err().unwrap();
    let Error::ClaimError(source) = &err else {
        panic!("expected a claim error, got {err:?}");
    };
    assert!(matches!(
        source.downcast_ref::<ClickhouseError>(),
        Some(ClickhouseError::ClaimerShutDown)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_claim_holds_nothing() {
    // D44: dropping the `claim` future releases anything it acquired. Cancel it at several
    // points, from before its first write to after its last.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v1 = Vault::new(store, claimer);

    let client = crate::server().with_database(&db);
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(store, claimer);

    // A key per round, two levels deep so cancellation can land between rows.
    for millis in [0, 1, 3, 6, 10, 20, 50, 200] {
        let k = format!("k{millis}/x");
        let _ = tokio::time::timeout(
            Duration::from_millis(millis),
            v1.claim(Key::new(&k).unwrap()),
        )
        .await;
        assert!(!refused(&v1, &k).await, "cancelled after {millis}ms");
        assert!(
            eventually_claimable(&v2, &k).await,
            "cancelled after {millis}ms"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_claims_on_one_key_admit_exactly_one() {
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let mut vaults = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
            .await
            .unwrap();
        let claimer = ClickhouseClaimer::with_options(
            client,
            ClickhouseClaimerOptions::default().root_path(&root),
        )
        .await
        .unwrap();
        vaults.push(Vault::new(store, claimer));
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
            .all(|e| matches!(e, Error::AlreadyClaimed { .. })),
        "{:?}",
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sibling_claims_all_succeed() {
    // Sixteen claims beneath one parent all add themselves to the parent's shared holders at
    // once: the version-checked updates collide and are retried, and every claim succeeds.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let mut vaults = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
            .await
            .unwrap();
        let claimer = ClickhouseClaimer::with_options(
            client,
            ClickhouseClaimerOptions::default().root_path(&root),
        )
        .await
        .unwrap();
        vaults.push(Vault::new(store, claimer));
    }

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let tasks: Vec<_> = (0..16)
        .map(|i| {
            let v = vaults[i % vaults.len()].clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                v.claim(Key::new(&format!("parent/{i}")).unwrap()).await
            })
        })
        .collect();
    let mut held = Vec::new();
    for task in tasks {
        held.push(task.await.unwrap().unwrap());
    }

    let holders = client
        .query("SELECT length(holders) FROM vault_claims WHERE key = 'parent'")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(holders, 16);
    assert!(refused(&vaults[0], "parent").await);
    drop(held);
    assert!(eventually_claimable(&vaults[0], "parent").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_claimer_works_with_any_store() {
    // Here a byte store: `ClickhouseClaimer` only needs the server for its own tables.
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");

    let claimer = ClickhouseClaimer::with_options(
        client.clone(),
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v1 = Vault::new(MemoryStore::new(), claimer);
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v2 = Vault::new(MemoryStore::new(), claimer);

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
        let root = format!("/lib_vault_tests/{db}");
        let client = crate::server().with_database(&db);
        let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
            .await
            .unwrap();
        let claimer = ClickhouseClaimer::with_options(
            client,
            ClickhouseClaimerOptions::default()
                .root_path(&root)
                .lease(Duration::from_secs(1)),
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
    let (client, db) = fresh_database().await;
    let root = format!("/lib_vault_tests/{db}");
    let store = ClickhouseStore::new(client.clone(), AppDispatcher::new(&root))
        .await
        .unwrap();
    let claimer = ClickhouseClaimer::with_options(
        client,
        ClickhouseClaimerOptions::default().root_path(&root),
    )
    .await
    .unwrap();
    let v = Vault::new(store, claimer);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "claims::child_holds_claim",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LV_CHILD_DB", &db)
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

    // `kill -9` gives the child no chance to release: its claim stops counting once its 1 s
    // lease runs out, and the value it committed is there.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(eventually_claimable(&v, "shared").await);
    let mut c = v.claim(Key::new("shared").unwrap()).await.unwrap();
    let doc: Doc = c.open_read(None).await.unwrap().try_as().unwrap();
    assert_eq!(doc.title, "from the child");
}
