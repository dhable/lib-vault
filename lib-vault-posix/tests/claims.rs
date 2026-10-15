//! Claim exclusion with lock files (D16, D44, D59, D67): within a process, between independent
//! stores on one root, and between processes.

mod common;

use common::*;
use lib_vault::Error;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

async fn refused(v: &FsVault, k: &str) -> bool {
    match v.claim(key(k)).await {
        Err(Error::AlreadyClaimed { key: refused }) => {
            assert_eq!(refused, key(k));
            true
        }
        Err(other) => panic!("unexpected error for {k}: {other:?}"),
        Ok(_) => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_claims_conflict_in_both_directions() {
    let v = TempVault::new();
    let held = v.claim("a/b").await;
    assert!(refused(&v.vault, "a/b").await);
    assert!(refused(&v.vault, "a/b/c").await); // descendant
    assert!(refused(&v.vault, "a").await); // ancestor
    assert!(!refused(&v.vault, "a/c").await); // sibling
    assert!(!refused(&v.vault, "a/bc").await); // shares only a string prefix
    drop(held);
    assert!(!refused(&v.vault, "a").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_and_drop_free_the_key_immediately() {
    let v = TempVault::new();
    v.claim("a").await.release().unwrap();
    assert!(!refused(&v.vault, "a").await);
    drop(v.claim("a").await);
    assert!(!refused(&v.vault, "a/b").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn independent_stores_on_one_root_exclude_each_other() {
    // Separate `FsStore`s open separate descriptors; `flock` still excludes them.
    let v = TempVault::new();
    let other = v.reopen();
    let _held = v.claim("a").await;
    assert!(refused(&other, "a/b").await);
    assert!(!refused(&other, "b").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claimed_key_can_be_written_by_its_holder_only() {
    let v = TempVault::new();
    let mut c = v.claim("doc").await;
    write(&mut c, None, b"mine").await;
    assert!(refused(&v.vault, "doc").await);
    c.release().unwrap();
    let mut c = v.claim("doc").await;
    assert_eq!(read(&mut c, None).await.unwrap(), b"mine");
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
        let vault = fs_vault(&root, Default::default());
        let _claim = vault.claim(key(&k)).await.unwrap();
        println!("LV_CHILD_HELD");
        std::thread::sleep(std::time::Duration::from_secs(60));
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_exclude_other_processes() {
    let v = TempVault::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "child_holds_claim",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LV_CHILD_ROOT", &v.root)
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

    assert!(refused(&v.vault, "shared").await);
    assert!(refused(&v.vault, "shared/x").await);

    // `kill -9` releases the child's locks: the kernel closes its descriptors.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!refused(&v.vault, "shared").await);
}
