use futures::future::{self, BoxFuture};
use lib_vault::{ClaimLock, Claimer, Error, Key};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

type Held = Arc<Mutex<HashSet<Key>>>;

/// A [`Claimer`] that keeps its lock table in memory.
///
/// Clones share the same table, so several `Vault`s using clones of one claimer exclude each
/// other. Exclusion is limited to that table: separate `MemoryClaimer`s, and other processes,
/// don't see each other's claims.
///
/// Conflicts follow the `Claimer` contract: a held key blocks itself, its descendants and its
/// ancestors, compared component-wise. `try_lock` never waits, and a released lock is free
/// again immediately. It works with any `Store`, since it never touches stored data.
#[derive(Clone, Default)]
pub struct MemoryClaimer {
    held: Held,
}

impl MemoryClaimer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Claimer for MemoryClaimer {
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
        let mut held = self.held.lock().expect("lock poisoned");
        let conflict = held
            .iter()
            .any(|h| h.starts_with(key) || key.starts_with(h));
        let result = if conflict {
            Err(Error::AlreadyClaimed { key: key.clone() })
        } else {
            held.insert(key.clone());
            Ok(Box::new(MemoryLock {
                key: key.clone(),
                held: Some(self.held.clone()),
            }) as Box<dyn ClaimLock>)
        };
        Box::pin(future::ready(result))
    }
}

// One held key. Removed from the table by `release`, or by `Drop` as a fallback.
struct MemoryLock {
    key: Key,
    held: Option<Held>,
}

impl MemoryLock {
    fn unlock(&mut self) {
        if let Some(held) = self.held.take() {
            held.lock().expect("lock poisoned").remove(&self.key);
        }
    }
}

impl ClaimLock for MemoryLock {
    fn release(mut self: Box<Self>) -> Result<(), Error> {
        self.unlock();
        Ok(())
    }
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        self.unlock();
    }
}
