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
