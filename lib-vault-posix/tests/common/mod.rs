#![allow(dead_code)]

use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{AsKeyRef, Claim, Error, Key, Vault, rt::SharedMode};
use lib_vault_posix::{FsClaimer, FsStore, FsStoreOptions, PosixError};
use std::path::{Path, PathBuf};

pub type FsVault = Vault<SharedMode>;
pub type FsClaim = Claim<SharedMode>;

/// A `Vault` over an `FsStore` at `root` with its `FsClaimer`.
pub fn fs_vault(root: impl AsRef<Path>, options: FsStoreOptions) -> FsVault {
    let store = FsStore::new(root, options).unwrap();
    let claimer = FsClaimer::new(&store);
    Vault::new(store, claimer)
}

pub fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

/// A vault in a fresh temporary directory, removed when dropped.
pub struct TempVault {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub vault: FsVault,
}

impl TempVault {
    pub fn new() -> Self {
        Self::with(FsStoreOptions::default())
    }

    pub fn with(options: FsStoreOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vault");
        let vault = fs_vault(&root, options);
        let root = std::fs::canonicalize(&root).unwrap();
        TempVault { dir, root, vault }
    }

    /// A second, independent `Vault` (its own store, claimer and descriptors) on the same root.
    pub fn reopen(&self) -> FsVault {
        fs_vault(&self.root, FsStoreOptions::default())
    }

    pub async fn claim(&self, k: &str) -> FsClaim {
        self.vault.claim(key(k)).await.unwrap()
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

pub async fn write(claim: &mut FsClaim, k: impl AsKeyRef, data: &[u8]) {
    let mut writer = claim.open_write(k).await.unwrap();
    writer.write_all(data).await.unwrap();
    writer.close().await.unwrap();
}

pub async fn read(claim: &mut FsClaim, k: impl AsKeyRef) -> Result<Vec<u8>, Error> {
    let mut reader = claim.open_read(k).await?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await.unwrap();
    Ok(buf)
}

pub async fn list(claim: &mut FsClaim, k: impl AsKeyRef) -> Vec<String> {
    let mut keys: Vec<String> = claim
        .list(k)
        .await
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    keys.sort();
    keys
}

/// The `PosixError` inside a store or claim error, if any.
pub fn posix(err: &Error) -> Option<&PosixError> {
    match err {
        Error::StoreError(e) | Error::ClaimError(e) => e.downcast_ref::<PosixError>(),
        _ => None,
    }
}

/// Every entry under `dir`, recursively, as paths relative to it (including hidden files).
pub fn tree(dir: &Path) -> Vec<String> {
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

pub fn open_store(root: &Path, options: FsStoreOptions) -> Result<FsStore, PosixError> {
    FsStore::new(root, options)
}
