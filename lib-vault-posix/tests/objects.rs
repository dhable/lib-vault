//! Object operations through a `Claim`: the `Store` contract (design §5, D40, D43, D48),
//! scoping (D36) and the case rule (D6), against the filesystem backend.

mod common;

use common::*;
use lib_vault::Error;
use lib_vault_posix::PosixError;
use std::os::unix::fs::PermissionsExt;

#[tokio::test(flavor = "multi_thread")]
async fn write_then_read_round_trips() {
    let v = TempVault::new();
    let mut c = v.claim("session").await;
    write(&mut c, "notes.md", b"hello").await;
    assert_eq!(read(&mut c, "notes.md").await.unwrap(), b"hello");
    // Multiple chunks larger than the writer's buffer.
    let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    write(&mut c, "big", &big).await;
    assert_eq!(read(&mut c, "big").await.unwrap(), big);
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_are_plain_files_at_their_key_path() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "b/c", b"deep").await;
    let file = v.path("a/b/c");
    assert_eq!(std::fs::read(&file).unwrap(), b"deep");
    // Modes are set explicitly, independent of the umask (§8.2).
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(v.path("a/b"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn key_forms_and_none_name_the_same_objects() {
    let v = TempVault::new();
    let mut c = v.claim("x/y").await;
    write(&mut c, None, b"self").await;
    assert_eq!(read(&mut c, None).await.unwrap(), b"self");
    c.release().unwrap();

    let mut parent = v.claim("x").await;
    assert_eq!(read(&mut parent, "y").await.unwrap(), b"self");
    assert_eq!(read(&mut parent, &key("y")).await.unwrap(), b"self");
    assert_eq!(read(&mut parent, Some(&key("y"))).await.unwrap(), b"self");
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_only_see_their_own_subtree() {
    let v = TempVault::new();
    let mut outside = v.claim("b").await;
    write(&mut outside, None, b"outside").await;
    outside.release().unwrap();

    let mut c = v.claim("a").await;
    assert!(matches!(
        read(&mut c, "b").await,
        Err(Error::NotFound { .. })
    ));
    assert!(list(&mut c, None).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_missing_object_reports_the_full_key() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    match read(&mut c, "missing").await {
        Err(Error::NotFound { key: k }) => assert_eq!(k, key("a/missing")),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overwriting_replaces_the_whole_object() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"first, and longer").await;
    write(&mut c, "f", b"second").await;
    assert_eq!(read(&mut c, "f").await.unwrap(), b"second");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_semantics() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "f", b"x").await;
    c.delete("f").await.unwrap();
    assert!(matches!(
        read(&mut c, "f").await,
        Err(Error::NotFound { .. })
    ));
    c.delete("missing").await.unwrap(); // like HTTP DELETE
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_none_deletes_the_claimed_object() {
    let v = TempVault::new();
    let mut c = v.claim("a/obj").await;
    write(&mut c, None, b"self").await;
    c.delete(None).await.unwrap();
    assert!(matches!(
        read(&mut c, None).await,
        Err(Error::NotFound { .. })
    ));
    assert!(!v.path("a/obj").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn writing_a_key_that_is_a_directory_is_refused() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "dir/f", b"x").await;
    let err = c.open_write("dir").await.err().unwrap();
    assert!(
        matches!(posix(&err), Some(PosixError::Unsupported { .. })),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_returns_relative_keys_by_whole_components() {
    let v = TempVault::new();
    let mut c = v.claim("root").await;
    for k in ["x", "dir/y", "dir/sub/z", "dirt/w"] {
        write(&mut c, k, b"_").await;
    }
    assert_eq!(
        list(&mut c, None).await,
        ["dir/sub/z", "dir/y", "dirt/w", "x"]
    );
    assert_eq!(list(&mut c, "dir").await, ["sub/z", "y"]);
    assert!(list(&mut c, "di").await.is_empty());
    assert!(list(&mut c, "x").await.is_empty()); // a file has nothing beneath it
    assert!(list(&mut c, "nothing").await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hides_reserved_files_and_foreign_names() {
    let v = TempVault::new();
    let mut c = v.claim("root").await;
    write(&mut c, "kept", b"_").await;
    // Lock files exist for held ancestors; a foreign name that isn't a valid key is skipped.
    std::fs::write(v.path("root/bad*name"), b"_").unwrap();
    assert_eq!(list(&mut c, None).await, ["kept"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_string_keys_are_rejected() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    for bad in ["../x", "b//c", "CON", ".vault-lock-b", "bad*char"] {
        let err = c.open_write(bad).await.err().expect(bad);
        assert!(matches!(err, Error::Key(_)), "{bad:?}: {err:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn case_collisions_are_refused() {
    let v = TempVault::new();
    let mut c = v.claim("a").await;
    write(&mut c, "Notes.md", b"x").await;
    let err = c.open_write("notes.md").await.err().unwrap();
    assert!(
        matches!(posix(&err), Some(PosixError::CaseCollision { .. })),
        "{err:?}"
    );
    write(&mut c, "Notes.md", b"y").await; // same spelling: an overwrite
    assert_eq!(read(&mut c, "Notes.md").await.unwrap(), b"y");
}
