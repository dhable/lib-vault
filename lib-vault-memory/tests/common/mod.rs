#![allow(dead_code)]

use futures::{AsyncReadExt, AsyncWriteExt};
use lib_vault::{AsKeyRef, Claim, Error, Key, Vault, rt};
use lib_vault_memory::{MemoryClaimer, MemoryStore};

pub use futures::executor::block_on;

pub fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

pub fn vault() -> Vault<rt::SharedMode> {
    Vault::new(MemoryStore::new(), MemoryClaimer::new())
}

pub async fn claim(vault: &Vault<rt::SharedMode>, k: &str) -> Claim<rt::SharedMode> {
    vault.claim(key(k)).await.unwrap()
}

/// Writes `data` and closes the writer, which commits it.
pub async fn write(claim: &mut Claim<rt::SharedMode>, k: impl AsKeyRef, data: &[u8]) {
    let mut writer = claim.open_write(k).await.unwrap();
    writer.write_all(data).await.unwrap();
    writer.close().await.unwrap();
}

pub async fn read(claim: &mut Claim<rt::SharedMode>, k: impl AsKeyRef) -> Result<Vec<u8>, Error> {
    let mut reader = claim.open_read(k).await?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await.unwrap();
    Ok(buf)
}

pub async fn list(claim: &mut Claim<rt::SharedMode>, k: impl AsKeyRef) -> Vec<String> {
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
