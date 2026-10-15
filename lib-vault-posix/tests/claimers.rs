//! `FsStore` works with any `Claimer` (D75): here a `MemoryClaimer`, standing in for a
//! distributed claimer. The store must not rely on anything `FsClaimer` does.

mod common;

use common::*;
use lib_vault::{Error, Vault};
use lib_vault_memory::MemoryClaimer;
use lib_vault_posix::{FsStore, FsStoreOptions};

fn memory_claimed_vault() -> (tempfile::TempDir, std::path::PathBuf, FsVault) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    let store = FsStore::new(&root, FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    (dir, root, Vault::new(store, MemoryClaimer::new()))
}

#[tokio::test(flavor = "multi_thread")]
async fn objects_work_without_fs_claimer() {
    let (_dir, root, vault) = memory_claimed_vault();
    let mut c = vault.claim(key("a/b")).await.unwrap();
    write(&mut c, "deep/f", b"data").await; // the store creates its own directories
    assert_eq!(read(&mut c, "deep/f").await.unwrap(), b"data");
    assert_eq!(list(&mut c, None).await, ["deep/f"]);
    c.delete("deep/f").await.unwrap();
    assert!(matches!(
        read(&mut c, "deep/f").await,
        Err(Error::NotFound { .. })
    ));
    c.release().unwrap();
    assert!(
        tree(&root).iter().all(|p| !p.contains(".vault-lock-")),
        "{:?}",
        tree(&root)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicts_come_from_the_chosen_claimer() {
    let (_dir, _root, vault) = memory_claimed_vault();
    let held = vault.claim(key("a")).await.unwrap();
    assert!(matches!(
        vault.claim(key("a/b")).await,
        Err(Error::AlreadyClaimed { .. })
    ));
    drop(held);
    vault.claim(key("a/b")).await.unwrap();
}
