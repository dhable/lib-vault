//! Claim exclusion with lock files (D16, D44, D59, D67): within a process, between independent
//! stores on one root, and between processes.

use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{Error, Key, Vault, io_driver::AsyncIoDriver};
use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

async fn refused(v: &Vault<AsyncIoDriver>, k: &str) -> bool {
    match v.claim(Key::new(k).unwrap()).await {
        Err(Error::AlreadyClaimed { key: refused }) => {
            assert_eq!(refused, Key::new(k).unwrap());
            true
        }
        Err(other) => panic!("unexpected error for {k}: {other:?}"),
        Ok(_) => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_claims_conflict_in_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
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
async fn release_and_drop_free_the_key_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let c = v.claim(Key::new("a").unwrap()).await.unwrap();
    c.release().unwrap();
    assert!(!refused(&v, "a").await);
    let c = v.claim(Key::new("a").unwrap()).await.unwrap();
    drop(c);
    assert!(!refused(&v, "a/b").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_stores_on_one_root_exclude_each_other() {
    // Separate `FsStore`s open separate descriptors; `flock` still excludes them.
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);

    let other_store = FsStore::new(&root, FsStoreOptions::default()).unwrap();
    let other_claimer = FsClaimer::new(&other_store);
    let other = Vault::new(other_store, other_claimer);

    let _held = v.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(refused(&other, "a/b").await);
    assert!(!refused(&other, "b").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claimed_key_can_be_written_by_its_holder_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("doc").unwrap()).await.unwrap();

    let mut w = c.open_write(None).await.unwrap();
    w.write_all(b"mine").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    assert!(refused(&v, "doc").await);
    c.release().unwrap();

    let mut c = v.claim(Key::new("doc").unwrap()).await.unwrap();
    let mut buf = Vec::new();
    let mut r = c.open_read(None).await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"mine");
}

/// Run as a child process by `claims_exclude_other_processes`: holds a claim until killed.
#[test]
#[ignore = "helper for claims_exclude_other_processes"]
fn child_holds_claim() {
    let (Ok(root), Ok(k)) = (
        std::env::var("LV_CHILD_ROOT"),
        std::env::var("LV_CHILD_KEY"),
    ) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let store = FsStore::new(&root, FsStoreOptions::default()).unwrap();
        let claimer = FsClaimer::new(&store);
        let vault = Vault::new(store, claimer);
        let _claim = vault.claim(Key::new(&k).unwrap()).await.unwrap();
        println!("LV_CHILD_HELD");
        std::thread::sleep(std::time::Duration::from_secs(60));
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_exclude_other_processes() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_holds_claim",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LV_CHILD_ROOT", &root)
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

    // `kill -9` releases the child's locks: the kernel closes its descriptors.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!refused(&v, "shared").await);
}
