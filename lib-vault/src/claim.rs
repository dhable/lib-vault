use crate::{
    AsKeyRef, ClaimLock, Error, Key, ObjectReader, ObjectWriter, ScopedStore, Store, VaultState,
};
use std::sync::{Arc, Mutex};
use std::task::Waker;

/// A `Claim` provides exclusive access to objects stored at a key and any child keys. Holding
/// a `Claim` is the only way to list, delete, read and write objects.
///
/// `Claim` is safe to move between tasks, making it async compatable with runtimes like tokio
/// (e.g. `Claim` implements `Send`). Although it is not possible to use a shared `Claim` for
/// any purpose (e.g. `Claim` does not impelemtn `Sync`). This is done by requiring a mutable
/// reference to use any method on the `Claim`. It does not mean that every method changes the
/// state of the object.
pub struct Claim {
    store: ScopedStore,
    lock: Option<Box<dyn ClaimLock>>,
    state: Arc<Mutex<VaultState>>,
}

impl Claim {
    // Create a new claim instance with the `Store` keys scoped to `base`. Intended
    // to be used by `Vault` to create a claim after a claim's lock is obtained.
    pub(crate) fn new(store: Arc<dyn Store>, base: Key, state: Arc<Mutex<VaultState>>) -> Self {
        let store = ScopedStore::new(base, store);
        let lock = None;
        Self { store, lock, state }
    }

    // Helper method that allows setting the lock object after construction. We do this
    // so the state count of claim objects can run the drop logic even if the Claimer
    // fails to obtain a lock.
    pub(crate) fn set_lock(&mut self, lock: Box<dyn ClaimLock>) {
        if self.lock.is_none() {
            self.lock = Some(lock);
        }
    }

    /// Lists objects stored at the `base` + `prefix` key and the children. Returns
    /// a list of matching object keys. For the first revision, this method does not
    /// support paging or limiting options.
    ///
    /// Example:
    /// ```ignore
    ///    let claim = vault.claim(Key::new("top")?)?;
    ///    let list = claim.list("parent").await?;
    /// ```
    /// Would list all the objects that have keys that are children of `top/parent`.
    /// `None` lists everything in the claim.
    pub async fn list(&mut self, prefix: impl AsKeyRef) -> Result<Vec<Key>, Error> {
        let prefix = prefix.into_key()?;
        let resolved_key = self.store.resolve(prefix.as_ref())?;
        self.store.inner.list(&resolved_key).await
    }

    /// Deletes a specific object stored at `base` + `key`. Calling this method with a
    /// key that does not exist in the store is considered a successful delete operation.
    /// This behavior matches the semantics of the HTTP DELETE method.
    /// `None` means the claimed key itself.
    pub async fn delete(&mut self, key: impl AsKeyRef) -> Result<(), Error> {
        let key = key.into_key()?;
        let resolved_key = self.store.resolve(key.as_ref())?;
        self.store.inner.delete(&resolved_key).await
    }

    /// Opens a specific object at `base` + `key` for reading. The resulting reader is valid
    /// for the duration of the claim. Reading a key that does not point an object will return
    /// an error. `None` means the claimed key itself.
    pub async fn open_read<'a>(
        &'a mut self,
        key: impl AsKeyRef,
    ) -> Result<ObjectReader<'a>, Error> {
        let key = key.into_key()?;
        let resolved_key = self.store.resolve(key.as_ref())?;
        self.store.inner.open_read(&resolved_key).await
    }

    /// Opens a specific object at `base` + `key` for writing. The specific beahviors around when
    /// data is committed/flushed and what happens with in flight requests after the Claim is
    /// released should be documented by the the `Store` implementation. Due to the mutable borrowing
    /// and the lifetimes, there can only be a single open reader or writer at a given time.
    ///
    /// Writing to a key pointing at an existing object should overwrite the exiting object data.
    /// Writing to a key that does not have an object will create the object with the given key.
    /// `None` means the claimed key itself.
    pub async fn open_write<'a>(
        &'a mut self,
        key: impl AsKeyRef,
    ) -> Result<ObjectWriter<'a>, Error> {
        let key = key.into_key()?;
        let resolved_key = self.store.resolve(key.as_ref())?;
        self.store.inner.open_write(&resolved_key).await
    }

    /// Releases the underyling Claim lock issued when the Claim was created. Use the
    /// `release` method here instead of relying on `drop` in order to inspect the
    /// status and/or handle any resulting errors.
    pub fn release(mut self) -> Result<(), Error> {
        release_lock(&mut self)
    }
}

// Helper function that implements the resuable logic around taking the lock from
// the strut and calling release on it. Must be callable from both the `Drop`
// implementation as well as `Claim::release`.
fn release_lock(claim: &mut Claim) -> Result<(), Error> {
    if let Some(lock) = claim.lock.take() {
        lock.release()?;
    }
    Ok(())
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Attempt to release the lock if it hasn't been. Funnels errors to the
        // tracing crate as a best attempt to expose errors.
        let _ = release_lock(self)
            .map_err(|e| tracing::error!(cause = %e, "ClaimLock::release failed"));

        // Now notify any waiting threads if the new outstanding claim count is 0. Make sure
        // to actually trigger the wake outside of the mutex lock since it might trigger the
        // polling loop in Vault and that code tries to obtain a mutex lock itself.
        let waiters = {
            let mut state = self.state.lock().expect("lock poisoned");
            state.outstanding_claims -= 1;
            match state.outstanding_claims {
                0 => std::mem::take(&mut state.waiters),
                _ => Vec::new(),
            }
        };
        waiters.into_iter().for_each(Waker::wake);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod claim_tests {
        use super::*;

        #[test]
        fn test_send_marker() {
            // This test is just a compile time guard to ensure that `Claim` will implement `Send`.
            // This is our guarantee that changes to `Claim` do not elminate this implict marker.
            const _: () = {
                const fn assert_send<T: Send>() {}
                assert_send::<Claim>()
            };

            // `Vault` is shared across threads, and its futures must stay spawnable on
            // multi-threaded runtimes (design §13).
            fn assert_vault<T: Send + Sync + Clone>() {}
            assert_vault::<crate::Vault>();

            fn assert_send_value<T: Send>(_: &T) {}
            fn futures_are_send(vault: crate::Vault) {
                assert_send_value(&vault.claim(crate::Key::new("a").unwrap()));
                assert_send_value(&vault.shutdown());
            }
            let _ = futures_are_send;
        }
    }
}
