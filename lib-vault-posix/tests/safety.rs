//! Links, special files (D17) and the construction-time checks on the vault root (§9, D64,
//! D70).

use futures::AsyncWriteExt;
use lib_vault::{Error, Key, Vault};
use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions, PosixError, Sharing};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

fn unsupported(err: &Error) -> bool {
    matches!(err, Error::StoreError(e) if matches!(
        e.downcast_ref::<PosixError>(),
        Some(PosixError::Unsupported { .. })
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn symlinked_objects_are_refused_and_not_listed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write("real").await.unwrap();
    w.write_all(b"x").await.unwrap();
    w.close().await.unwrap();
    drop(w);
    let outside = dir.path().join("secret");
    std::fs::write(&outside, b"outside the vault").unwrap();
    symlink(&outside, root.join("a/link")).unwrap();

    let err = c.open_read("link").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
    let err = c.open_write("link").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
    let keys = c.list(None).await.unwrap();
    assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["real"]);
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside the vault");
}

#[tokio::test(flavor = "multi_thread")]
async fn symlinked_directories_are_never_followed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f"), b"outside").unwrap();
    std::fs::create_dir(root.join("a")).unwrap();
    symlink(&outside, root.join("a/dir")).unwrap();

    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let err = c.open_read("dir/f").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
    let err = c.open_write("dir/g").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
    assert!(!outside.join("g").exists());
    // Claiming through the symlink fails too.
    drop(c);
    assert!(v.claim(Key::new("a/dir/f").unwrap()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn hard_linked_objects_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write("f").await.unwrap();
    w.write_all(b"x").await.unwrap();
    w.close().await.unwrap();
    drop(w);
    std::fs::hard_link(root.join("a/f"), dir.path().join("alias")).unwrap();
    let err = c.open_read("f").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn fifos_are_refused_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path().join("vault"), FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    let claimer = FsClaimer::new(&store);
    let v = Vault::new(store, claimer);
    let mut c = v.claim(Key::new("a").unwrap()).await.unwrap();
    let mut w = c.open_write("f").await.unwrap(); // creates the directory
    w.write_all(b"x").await.unwrap();
    w.close().await.unwrap();
    drop(w);
    let status = std::process::Command::new("mkfifo")
        .arg(root.join("a/pipe"))
        .status()
        .unwrap();
    assert!(status.success());
    let err = c.open_read("pipe").await.err().unwrap();
    assert!(unsupported(&err), "{err:?}");
}

fn root_error(root: &Path, options: FsStoreOptions) -> PosixError {
    match FsStore::new(root, options) {
        Err(e) => e,
        Ok(_) => panic!("expected {} to be refused", root.display()),
    }
}

#[test]
fn the_root_must_not_be_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    symlink(dir.path().join("real"), dir.path().join("vault")).unwrap();
    let err = root_error(&dir.path().join("vault"), FsStoreOptions::default());
    assert!(matches!(err, PosixError::InvalidRoot { .. }), "{err}");
}

#[test]
fn system_directories_are_refused() {
    for root in ["/usr", "/tmp", "/"] {
        let err = root_error(
            Path::new(root),
            FsStoreOptions::default().strict_modes(false),
        );
        assert!(
            matches!(err, PosixError::InvalidRoot { .. }),
            "{root}: {err}"
        );
    }
}

#[test]
fn the_root_mode_must_match_the_sharing_mode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = root_error(&root, FsStoreOptions::default());
    assert!(matches!(err, PosixError::InvalidRoot { .. }), "{err}");

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    FsStore::new(&root, FsStoreOptions::default()).unwrap();
}

#[test]
fn a_new_root_is_created_with_the_sharing_mode() {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    FsStore::new(&private, FsStoreOptions::default()).unwrap();
    assert_eq!(
        std::fs::metadata(&private).unwrap().permissions().mode() & 0o7777,
        0o700
    );

    // The effective gid: a group the process can always give its own files.
    let gid = rustix::process::getegid().as_raw();
    let shared = dir.path().join("shared");
    FsStore::new(
        &shared,
        FsStoreOptions::default().sharing(Sharing::Group(gid)),
    )
    .unwrap();
    let meta = std::fs::metadata(&shared).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o2770);
    // Reopening in private mode is refused: the root is shared.
    let err = root_error(&shared, FsStoreOptions::default());
    assert!(matches!(err, PosixError::InvalidRoot { .. }), "{err}");
}

#[test]
fn strict_modes_refuse_a_group_or_world_writable_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let open = dir.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let root = open.join("vault");

    let err = root_error(&root, FsStoreOptions::default());
    match err {
        PosixError::StrictModes { path, .. } => {
            assert_eq!(path, std::fs::canonicalize(&open).unwrap())
        }
        other => panic!("expected StrictModes, got {other}"),
    }
    // With strict modes off, the same root is accepted.
    FsStore::new(&root, FsStoreOptions::default().strict_modes(false)).unwrap();
}

#[test]
fn a_root_with_extended_acls_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    FsStore::new(&root, FsStoreOptions::default()).unwrap();

    let added = if cfg!(target_vendor = "apple") {
        std::process::Command::new("chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&root)
            .status()
    } else {
        std::process::Command::new("setfacl")
            .args(["-m", "u:root:rx"])
            .arg(&root)
            .status()
    };
    if !added.is_ok_and(|s| s.success()) {
        eprintln!(
            "skipping: couldn't add an ACL (no chmod +a / setfacl, or unsupported filesystem)"
        );
        return;
    }
    let err = root_error(&root, FsStoreOptions::default());
    assert!(matches!(err, PosixError::ExtendedAcl { .. }), "{err}");
    if cfg!(target_vendor = "apple") {
        let _ = std::process::Command::new("chmod")
            .arg("-N")
            .arg(&root)
            .status();
    }
}
