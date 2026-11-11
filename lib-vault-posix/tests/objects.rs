//! Object operations through a `Claim`: the `Store` contract (design §5, D40, D43, D48),
//! scoping (D36) and the case rule (D6), against the filesystem backend.

use lib_vault::{Error, Key, Vault};
use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions, PosixError};
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread")]
async fn write_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("session").unwrap()).await.unwrap();

    c.store("notes.md", b"hello".to_vec()).await.unwrap();

    let buf: Vec<u8> = c.load("notes.md").await.unwrap();
    assert_eq!(buf, b"hello");

    let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    c.store("big", big.clone()).await.unwrap();

    let buf: Vec<u8> = c.load("big").await.unwrap();
    assert_eq!(buf, big);
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_are_plain_files_at_their_key_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("b/c", b"deep".to_vec()).await.unwrap();

    let file = root.join("a/b/c");
    assert_eq!(std::fs::read(&file).unwrap(), b"deep");
    // Modes are set explicitly, independent of the umask (§8.2).
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(root.join("a/b"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn key_forms_and_none_name_the_same_objects() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("x/y").unwrap()).await.unwrap();

    c.store(None, b"self".to_vec()).await.unwrap();

    let buf: Vec<u8> = c.load(None).await.unwrap();
    assert_eq!(buf, b"self");
    c.release().unwrap();

    let mut parent = v.claim(Key::new("x").unwrap()).await.unwrap();
    let y = Key::new("y").unwrap();

    let buf: Vec<u8> = parent.load("y").await.unwrap();
    assert_eq!(buf, b"self");

    let buf: Vec<u8> = parent.load(&y).await.unwrap();
    assert_eq!(buf, b"self");

    let buf: Vec<u8> = parent.load(Some(&y)).await.unwrap();
    assert_eq!(buf, b"self");
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_only_see_their_own_subtree() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);

    let mut outside = v.claim(Key::new("b").unwrap()).await.unwrap();
    outside.store(None, b"outside".to_vec()).await.unwrap();
    outside.release().unwrap();

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(matches!(
        c.load::<Vec<u8>>("b").await,
        Err(Error::NotFound { .. })
    ));
    assert!(c.list(None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_missing_object_reports_the_full_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    match c.load::<Vec<u8>>("missing").await.err() {
        Some(Error::NotFound { key }) => assert_eq!(key, Key::new("a/missing").unwrap()),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overwriting_replaces_the_whole_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"first, and longer".to_vec()).await.unwrap();

    c.store("f", b"second".to_vec()).await.unwrap();

    let buf: Vec<u8> = c.load("f").await.unwrap();
    assert_eq!(buf, b"second");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("f", b"x".to_vec()).await.unwrap();

    c.delete("f").await.unwrap();
    assert!(matches!(
        c.load::<Vec<u8>>("f").await,
        Err(Error::NotFound { .. })
    ));
    c.delete("missing").await.unwrap(); // like HTTP DELETE
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_none_deletes_the_claimed_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a/obj").unwrap()).await.unwrap();

    c.store(None, b"self".to_vec()).await.unwrap();

    c.delete(None).await.unwrap();
    assert!(matches!(
        c.load::<Vec<u8>>(None).await,
        Err(Error::NotFound { .. })
    ));
    assert!(!root.join("a/obj").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn writing_a_key_that_is_a_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("dir/f", b"x".to_vec()).await.unwrap();

    let err = c.store("dir", b"x".to_vec()).await.err().unwrap();
    assert!(
        matches!(&err, Error::StoreError(e) if matches!(
            e.downcast_ref::<PosixError>(),
            Some(PosixError::Unsupported { .. })
        )),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_returns_relative_keys_by_whole_components() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();
    for k in ["x", "dir/y", "dir/sub/z", "dirt/w"] {
        c.store(k, b"_".to_vec()).await.unwrap();
    }

    // Listing order follows the directory, so sort before comparing.
    let mut keys = c.list(None).await.unwrap();
    keys.sort();
    assert_eq!(
        keys.iter().map(Key::as_str).collect::<Vec<_>>(),
        ["dir/sub/z", "dir/y", "dirt/w", "x"]
    );
    let mut keys = c.list("dir").await.unwrap();
    keys.sort();
    assert_eq!(
        keys.iter().map(Key::as_str).collect::<Vec<_>>(),
        ["sub/z", "y"]
    );
    assert!(c.list("di").await.unwrap().is_empty());
    assert!(c.list("x").await.unwrap().is_empty()); // a file has nothing beneath it
    assert!(c.list("nothing").await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hides_reserved_files_and_foreign_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("root").unwrap()).await.unwrap();

    c.store("kept", b"_".to_vec()).await.unwrap();

    // Lock files exist for held ancestors; a foreign name that isn't a valid key is skipped.
    std::fs::write(root.join("root/bad*name"), b"_").unwrap();
    let keys = c.list(None).await.unwrap();
    assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["kept"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_string_keys_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    for bad in ["../x", "b//c", "CON", ".vault-lock-b", "bad*char"] {
        let err = c.store(bad, b"x".to_vec()).await.expect_err(bad);
        assert!(matches!(err, Error::Key(_)), "{bad:?}: {err:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn case_collisions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();

    c.store("Notes.md", b"x".to_vec()).await.unwrap();

    let err = c.store("notes.md", b"x".to_vec()).await.err().unwrap();
    assert!(
        matches!(&err, Error::StoreError(e) if matches!(
            e.downcast_ref::<PosixError>(),
            Some(PosixError::CaseCollision { .. })
        )),
        "{err:?}"
    );

    // Same spelling: an overwrite.
    c.store("Notes.md", b"y".to_vec()).await.unwrap();

    let buf: Vec<u8> = c.load("Notes.md").await.unwrap();
    assert_eq!(buf, b"y");
}
