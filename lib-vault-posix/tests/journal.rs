//! Create-on-write and lock files (D67) and the write journal (D63).

use futures::{AsyncReadExt, AsyncWriteExt};
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
async fn a_committed_write_leaves_no_journal() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"data").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    assert!(!root.join("a/.vault-journal-f").exists());
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"data");
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_reach_the_object_only_on_close() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"old").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"new").await.unwrap();
    w.flush().await.unwrap();
    // Before close: the object is untouched and the journal holds the new data.
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"old");
    assert!(root.join("a/.vault-journal-f").exists());
    w.close().await.unwrap();
    drop(w);
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"new");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_writer_keeps_the_old_object_and_removes_its_journal() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"old").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"never committed").await.unwrap();
    drop(w);
    assert!(!root.join("a/.vault-journal-f").exists());

    let mut buf = Vec::new();
    let mut r = c.open_read("f").await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"old");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_blocks_only_that_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"old").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut w = c.open_write("g").await.unwrap();
    w.write_all(b"other").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    // Simulate a crash mid-write.
    let journal = root.join("a/.vault-journal-f");
    std::fs::write(&journal, b"partial").unwrap();

    for err in [
        c.open_read("f").await.err().unwrap(),
        c.open_write("f").await.err().unwrap(),
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
    let mut buf = Vec::new();
    let mut r = c.open_read("g").await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"other");
    let mut keys = c.list(None).await.unwrap();
    keys.sort();
    assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["f", "g"]);
    c.delete("f").await.unwrap();
    assert!(journal.exists(), "delete doesn't remove the journal");

    // Resolving by hand (deleting the journal) makes the key usable again.
    std::fs::remove_file(&journal).unwrap();
    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"fresh").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut buf = Vec::new();
    let mut r = c.open_read("f").await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"fresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_journaling_writes_go_straight_to_the_object() {
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

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"first, longer").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"second").await.unwrap();
    w.flush().await.unwrap();
    assert!(!root.join("a/.vault-journal-f").exists());
    // In place: truncated on open, data visible after the flush.
    assert_eq!(std::fs::read(root.join("a/f")).unwrap(), b"second");
    w.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_is_reported_even_without_journaling() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"old").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    std::fs::write(root.join("a/.vault-journal-f"), b"partial").unwrap();
    c.release().unwrap();

    let store = FsStore::new(&root, FsStoreOptions::default().journaling(false)).unwrap();
    let claimer = FsClaimer::new(&store);
    let unjournaled = Vault::new(store, claimer);
    let mut c = unjournaled.claim(Key::new("a").unwrap()).await.unwrap();
    let err = c.open_read("f").await.err().unwrap();
    assert!(
        matches!(&err, Error::StoreError(e) if matches!(
            e.downcast_ref::<PosixError>(),
            Some(PosixError::InterruptedWrite { .. })
        )),
        "{err:?}"
    );
    assert!(!matches!(err, Error::NotFound { .. }));
}
