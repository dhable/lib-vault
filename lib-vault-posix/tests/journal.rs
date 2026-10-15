//! Create-on-write and lock files (D67) and the write journal (D63).

mod common;

use common::*;
use futures::AsyncWriteExt;
use lib_vault::Error;
use lib_vault_posix::{FsStoreOptions, PosixError};

#[tokio::test(flavor = "multi_thread")]
async fn claiming_and_releasing_creates_no_object() {
    let v = TempVault::new();
    let c = v.claim("a/b/c").await;
    c.release().unwrap();
    // Only intermediate directories and ancestor lock files remain; no object, and the
    // target's own lock file was removed on release.
    let left = tree(&v.root);
    assert!(!v.path("a/b/c").exists(), "{left:?}");
    assert!(!v.path("a/b/.vault-lock-c").exists(), "{left:?}");
    assert!(
        left.iter()
            .all(|p| p == "a" || p == "a/b" || p.contains(".vault-lock-")),
        "{left:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_committed_write_leaves_no_journal() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"data").await;
    assert!(!v.path("a/.vault-journal-f").exists());
    assert_eq!(std::fs::read(v.path("a/f")).unwrap(), b"data");
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_reach_the_object_only_on_close() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"old").await;

    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"new").await.unwrap();
    w.flush().await.unwrap();
    // Before close: the object is untouched and the journal holds the new data.
    assert_eq!(std::fs::read(v.path("a/f")).unwrap(), b"old");
    assert!(v.path("a/.vault-journal-f").exists());
    w.close().await.unwrap();
    drop(w);
    assert_eq!(std::fs::read(v.path("a/f")).unwrap(), b"new");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_writer_keeps_the_old_object_and_removes_its_journal() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"old").await;
    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"never committed").await.unwrap();
    drop(w);
    assert!(!v.path("a/.vault-journal-f").exists());
    assert_eq!(read(&mut c, "f").await.unwrap(), b"old");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_blocks_only_that_object() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"old").await;
    write(&mut c, "g", b"other").await;
    // Simulate a crash mid-write.
    let journal = v.path("a/.vault-journal-f");
    std::fs::write(&journal, b"partial").unwrap();

    for err in [
        c.open_read("f").await.err().unwrap(),
        c.open_write("f").await.err().unwrap(),
    ] {
        match posix(&err) {
            Some(PosixError::InterruptedWrite { key: k, journal: j }) => {
                assert_eq!(*k, key("a/f"));
                assert_eq!(*j, journal);
            }
            other => panic!("expected InterruptedWrite, got {other:?} ({err})"),
        }
    }
    // Other objects, list and delete are unaffected.
    assert_eq!(read(&mut c, "g").await.unwrap(), b"other");
    assert_eq!(list(&mut c, None).await, ["f", "g"]);
    c.delete("f").await.unwrap();
    assert!(journal.exists(), "delete doesn't remove the journal");

    // Resolving by hand (deleting the journal) makes the key usable again.
    std::fs::remove_file(&journal).unwrap();
    write(&mut c, "f", b"fresh").await;
    assert_eq!(read(&mut c, "f").await.unwrap(), b"fresh");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_journaling_writes_go_straight_to_the_object() {
    let v = TempVault::with(FsStoreOptions::default().journaling(false));
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"first, longer").await;
    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"second").await.unwrap();
    w.flush().await.unwrap();
    assert!(!v.path("a/.vault-journal-f").exists());
    // In place: truncated on open, data visible after the flush.
    assert_eq!(std::fs::read(v.path("a/f")).unwrap(), b"second");
    w.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leftover_journal_is_reported_even_without_journaling() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"old").await;
    std::fs::write(v.path("a/.vault-journal-f"), b"partial").unwrap();
    c.release().unwrap();

    let unjournaled = fs_vault(&v.root, FsStoreOptions::default().journaling(false));
    let mut c = unjournaled.claim(key("a")).await.unwrap();
    let err = read(&mut c, "f").await.unwrap_err();
    assert!(
        matches!(posix(&err), Some(PosixError::InterruptedWrite { .. })),
        "{err:?}"
    );
    assert!(!matches!(err, Error::NotFound { .. }));
}
