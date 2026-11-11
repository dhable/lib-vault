//! Create-on-write and lock files (D67) and the write journal (D63).

use lib_vault::{Error, Key, Vault};
use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions, PosixError};
use std::path::Path;

/// Every entry under `dir`, recursively, as paths relative to it (including hidden files).
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            out.push(path.strip_prefix(base).unwrap().display().to_string());
            if path.is_dir() && !path.is_symlink() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn claiming_and_releasing_creates_no_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let c = v.claim(Key::new("a/b/c").unwrap()).await.unwrap();
    c.release().unwrap();
    // Only intermediate directories and ancestor lock files remain; no object, and the
    // target's own lock file was removed on release.
    let left = tree(&root);
    assert!(!root.join("a/b/c").exists(), "{left:?}");
    assert!(!root.join("a/b/.vault-lock-c").exists(), "{left:?}");
    assert!(
        left.iter()
            .all(|p| p == "a" || p == "a/b" || p.contains(".vault-lock-")),
        "{left:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_store_leaves_no_journal() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"data".to_vec()).await.unwrap();

    assert!(!root.join("a/.vault-journal-f").exists());
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"data");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_store_keeps_the_old_object_and_leaves_no_journal() {
    // D88: a `store` refused before its commit starts never touches the object.
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"old".to_vec()).await.unwrap();

    // Not a `Vec<u8>`: refused before anything is written.
    assert!(matches!(
        c.store("f", String::from("never stored")).await,
        Err(Error::WrongType { .. })
    ));
    // A case collision: refused by the checks that run before the journal is created.
    assert!(c.store("F", b"never stored".to_vec()).await.is_err());

    assert!(tree(&root).iter().all(|p| !p.contains(".vault-journal-")));
    let buf: Vec<u8> = c.load("f").await.unwrap();
    assert_eq!(buf, b"old");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_store_still_completes() {
    // D88: `store` runs as one task on the blocking pool, so dropping its future doesn't
    // interrupt the write: the object ends up whole, old or new, never partial.
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"old".to_vec()).await.unwrap();

    let big = vec![7u8; 4 * 1024 * 1024];
    {
        let pending = c.store("f", big.clone());
        // Polled once, so the blocking task is started, then dropped.
        let _ = tokio::time::timeout(std::time::Duration::ZERO, pending).await;
    }

    // The task finishes on its own: wait for the journal to go.
    let journal = root.join("a/.vault-journal-f");
    for _ in 0..500 {
        if !journal.exists() && std::fs::read(root.join("a/f")).unwrap().len() == big.len() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!journal.exists());
    let buf: Vec<u8> = c.load("f").await.unwrap();
    assert!(buf == big);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_blocks_only_that_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    c.store("f", b"old".to_vec()).await.unwrap();

    c.store("g", b"other".to_vec()).await.unwrap();

    // Simulate a crash mid-write.
    let journal = root.join("a/.vault-journal-f");
    std::fs::write(&journal, b"partial").unwrap();

    for err in [
        c.load::<Vec<u8>>("f").await.err().unwrap(),
        c.store("f", b"x".to_vec()).await.err().unwrap(),
    ] {
        let Error::StoreError(e) = &err else {
            panic!("expected a store error, got {err:?}");
        };
        match e.downcast_ref::<PosixError>() {
            Some(PosixError::InterruptedWrite { key: k, journal: j }) => {
                assert_eq!(*k, Key::new("a/f").unwrap());
                assert_eq!(*j, journal);
            }
            other => panic!("expected InterruptedWrite, got {other:?} ({err})"),
        }
    }
    // Other objects, list and delete are unaffected.
    let buf: Vec<u8> = c.load("g").await.unwrap();
    assert_eq!(buf, b"other");
    let mut keys = c.list(None).await.unwrap();
    keys.sort();
    assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["f", "g"]);
    c.delete("f").await.unwrap();
    assert!(journal.exists(), "delete doesn't remove the journal");

    // Resolving by hand (deleting the journal) makes the key usable again.
    std::fs::remove_file(&journal).unwrap();
    c.store("f", b"fresh".to_vec()).await.unwrap();

    let buf: Vec<u8> = c.load("f").await.unwrap();
    assert_eq!(buf, b"fresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_journaling_stores_go_straight_to_the_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(
        dir.path().join("vault"),
        FsStoreOptions::default().journaling(false),
    )
    .unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"first, longer".to_vec()).await.unwrap();

    c.store("f", b"second").await.unwrap_err(); // not a `Vec<u8>`: the object is untouched
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"first, longer");

    c.store("f", b"second".to_vec()).await.unwrap();
    // In place: truncated and rewritten, with no journal in between.
    assert!(tree(&root).iter().all(|p| !p.contains(".vault-journal-")));
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"second");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_is_reported_even_without_journaling() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"old".to_vec()).await.unwrap();

    std::fs::write(root.join("a/.vault-journal-f"), b"partial").unwrap();
    c.release().unwrap();

    let store = FsStore::new(&root, FsStoreOptions::default().journaling(false)).unwrap();
    let claimer = FsClaimer::new(&store);
    let unjournaled = Vault::new(store, claimer);
    let mut c = unjournaled.claim(Key::new("a").unwrap()).await.unwrap();
    let err = c.load::<Vec<u8>>("f").await.err().unwrap();
    assert!(
        matches!(&err, Error::StoreError(e) if matches!(
            e.downcast_ref::<PosixError>(),
            Some(PosixError::InterruptedWrite { .. })
        )),
        "{err:?}"
    );
    assert!(!matches!(err, Error::NotFound { .. }));
}
