//! `FsStore` works with any `Claimer` (D75): here a `MemoryClaimer`, standing in for a
//! distributed claimer. The store must not rely on anything `FsClaimer` does.

use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{Error, Key, Vault, io_driver::AsyncIoDriver};
use lib_vault_memory::MemoryClaimer;
use lib_vault_posix::{FsStore, FsStoreOptions};
use std::path::{Path, PathBuf};

fn memory_claimed_vault() -> (tempfile::TempDir, PathBuf, Vault<AsyncIoDriver>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    let store = FsStore::new(&root, FsStoreOptions::default()).unwrap();
    let root = store.root_path().to_owned();
    (dir, root, Vault::new(store, MemoryClaimer::new()))
}

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
async fn objects_work_without_fs_claimer() {
    let (_dir, root, vault) = memory_claimed_vault();
    let mut c = vault.claim(Key::new("a/b").unwrap()).await.unwrap();

    // The store creates its own directories.
    let mut w = c.open_write("deep/f").await.unwrap();
    w.write_all(b"data").await.unwrap();
    w.close().await.unwrap();
    drop(w);

    let mut buf = Vec::new();
    let mut r = c.open_read("deep/f").await.unwrap();
    r.read_to_end(&mut buf).await.unwrap();
    drop(r);
    assert_eq!(buf, b"data");
    let keys = c.list(None).await.unwrap();
    assert_eq!(keys.iter().map(Key::as_str).collect::<Vec<_>>(), ["deep/f"]);
    c.delete("deep/f").await.unwrap();
    assert!(matches!(
        c.open_read("deep/f").await,
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
    let held = vault.claim(Key::new("a").unwrap()).await.unwrap();
    assert!(matches!(
        vault.claim(Key::new("a/b").unwrap()).await,
        Err(Error::AlreadyClaimed { .. })
    ));
    drop(held);
    vault.claim(Key::new("a/b").unwrap()).await.unwrap();
}
