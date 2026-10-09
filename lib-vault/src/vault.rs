use crate::{Claim, Claimer, Error, Key, Store};
use std::future;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};

/// Internal helper type to hold the shutdown state and outstanding claim counts
/// in order to implement the Vault shutdown behavior.
#[derive(Default)]
pub(crate) struct VaultState {
    pub(crate) shutting_down: bool,
    pub(crate) outstanding_claims: usize,
    pub(crate) waiters: Vec<Waker>,
}

/// The Vault is the entry point to setting up storage and a locking strategy for
/// working with long lived data.
#[derive(Clone)]
pub struct Vault {
    store: Arc<dyn Store>,
    claimer: Arc<dyn Claimer>,
    state: Arc<Mutex<VaultState>>,
}

impl Vault {
    /// Creates a new Vault given an implementation of a storage engine `store` and
    /// a claim locking engine `claimer`.
    pub fn new(store: impl Store, claimer: impl Claimer) -> Self {
        Self {
            store: Arc::new(store),
            claimer: Arc::new(claimer),
            state: Arc::new(Mutex::new(Default::default())),
        }
    }

    /// Attempt to obtain a new claim from this vault's `Claimer` engine for a given
    /// object path asychronously. This can result in either a valid `Claim` object, which
    /// provides exclusive access to the path and it's subpaths. Can result in an error
    /// if a claim cannot be obtained.
    pub async fn claim(&self, key: Key) -> Result<Claim, Error> {
        let mut claim = {
            // Do all the state updates and drop the guard before an await boundry. This also
            // avoids a possible deadlock on a single threaded execution pool.
            let mut state = self.state.lock().expect("lock poisoned");
            if state.shutting_down {
                return Err(Error::ShuttingDown);
            }
            state.outstanding_claims += 1;
            Claim::new(self.store.clone(), key.clone(), self.state.clone())
        };
        let lock = self.claimer.try_lock(&key).await?;
        claim.set_lock(lock);
        Ok(claim)
    }

    /// Blocks until all outstanding claims have been released. Will prevent new Claims
    /// from being issued.
    pub async fn shutdown(self) {
        // Flip the shutting_down boolean and release the mutex so it can be obtained in polling.
        // Avoid holding the mutex between polling fn calls.
        self.state.lock().expect("lock poisoned").shutting_down = true;
        future::poll_fn(|cx| {
            let mut state = self.state.lock().expect("lock poisoned");
            if state.outstanding_claims == 0 {
                let waiters = std::mem::take(&mut state.waiters);
                waiters.into_iter().for_each(Waker::wake);
                return Poll::Ready(());
            }

            // Record the current caller's Waker to notify it to continue
            // but only if there isn't already a Waker for that task.
            if state.waiters.iter().all(|w| !cx.waker().will_wake(w)) {
                state.waiters.push(cx.waker().clone());
            }

            Poll::Pending
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClaimLock, ObjectReader, ObjectWriter};
    use futures::FutureExt;
    use futures::future::{self, BoxFuture};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A store the claim never uses in these tests.
    struct NullStore;

    impl Store for NullStore {
        fn list(&self, _: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
            Box::pin(future::ready(Ok(Vec::new())))
        }
        fn delete<'a>(&'a self, _: &Key) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(future::ready(Ok(())))
        }
        fn open_read<'a>(&'a self, k: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
            Box::pin(future::ready(Err(Error::NotFound { key: k.clone() })))
        }
        fn open_write<'a>(&'a self, k: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
            Box::pin(future::ready(Err(Error::NotFound { key: k.clone() })))
        }
    }

    // Grants every lock; each lock counts its releases and optionally fails them.
    struct CountingClaimer {
        releases: Arc<AtomicUsize>,
        fail: bool,
    }

    struct CountingLock {
        releases: Arc<AtomicUsize>,
        fail: bool,
    }

    impl ClaimLock for CountingLock {
        fn release(self: Box<Self>) -> Result<(), Error> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            match self.fail {
                true => Err(Error::ClaimError("release failed".into())),
                false => Ok(()),
            }
        }
    }

    impl Claimer for CountingClaimer {
        fn try_lock<'a>(&'a self, _: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
            let lock = CountingLock {
                releases: self.releases.clone(),
                fail: self.fail,
            };
            Box::pin(future::ready(Ok(Box::new(lock) as Box<dyn ClaimLock>)))
        }
    }

    // Never grants a lock.
    struct PendingClaimer;

    impl Claimer for PendingClaimer {
        fn try_lock<'a>(&'a self, _: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
            Box::pin(future::pending())
        }
    }

    fn counting_vault(fail: bool) -> (Vault, Arc<AtomicUsize>) {
        let releases = Arc::new(AtomicUsize::new(0));
        let claimer = CountingClaimer {
            releases: releases.clone(),
            fail,
        };
        (Vault::new(NullStore, claimer), releases)
    }

    fn claim(vault: &Vault) -> Claim {
        let key = Key::new("a").unwrap();
        vault.claim(key).now_or_never().unwrap().unwrap()
    }

    #[test]
    fn lock_is_released_exactly_once() {
        let (vault, releases) = counting_vault(false);
        claim(&vault).release().unwrap(); // release, then the implicit drop
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        drop(claim(&vault)); // drop only
        assert_eq!(releases.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn release_errors_surface_and_still_count_down() {
        let (vault, releases) = counting_vault(true);
        assert!(matches!(claim(&vault).release(), Err(Error::ClaimError(_))));
        drop(claim(&vault)); // the error is only logged
        assert_eq!(releases.load(Ordering::SeqCst), 2);
        assert!(vault.shutdown().now_or_never().is_some());
    }

    #[test]
    fn cancelled_claim_gives_its_count_back() {
        // D47: dropping a `claim()` future mid-`try_lock` must not leave a count behind.
        let vault = Vault::new(NullStore, PendingClaimer);
        let mut pending = Box::pin(vault.claim(Key::new("a").unwrap()));
        assert!((&mut pending).now_or_never().is_none());
        drop(pending);
        assert!(vault.shutdown().now_or_never().is_some());
    }
}
