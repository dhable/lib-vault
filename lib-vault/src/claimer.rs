use crate::{Error, Key};
use futures::future::BoxFuture;

/// Sentinal object returned by `Claimer` to denote an exclusive access a key and its children
/// in the `Store`. This sentinal value is only intended to be consumed by `Claim` as part of
/// its release lifecycle.
pub trait ClaimLock: Send {
    /// Perform any cleanup and state adjustments in order to release the lock in the `Claimer`
    /// implementation. Implementations that require blocking operations or I/O should use new
    /// threads or particular async runtime constructs to perform the release in a non-blocking
    /// manner.
    ///
    /// Implementations should only return `Ok(())` if the underlying key lock has been releases
    /// such that `claim` can be called immediatly after `Claim::release` on the same key and be
    /// grated a new lock.This may be triggered  in user code with `Claim::release` or when the
    /// `Claim` is dropped.
    fn release(self: Box<Self>) -> Result<(), Error>;
}

/// A strategy for making claims exclusive: a `Claimer` decides whether a key may be claimed,
/// and holds a lock on it while the claim is alive.
///
/// The `Vault` calls [`Claimer::try_lock`] when a caller asks for a claim. If it succeeds,
/// the vault wraps the returned [`ClaimLock`] in a [`Claim`](crate::Claim) scoped to the key. A `Claimer`
/// only locks; it never touches object data, which belongs to the [`Store`](crate::Store).
///
/// # Exclusion
///
/// A claim on `k` covers `k` and every key beneath it, compared component-wise
/// ([`Key::starts_with`]): a claim on `a/b` covers `a/b`, `a/b/c` and `a/b/c/d`, but not
/// `a/bc` or `a/z`. Two claims conflict when one covers the other's key, so while `a/b` is
/// held, `try_lock` must refuse all of:
/// - `a/b` itself;
/// - any descendant, such as `a/b/c`;
/// - any ancestor, such as `a`.
///
/// Disjoint keys such as `a/b` and `a/c` must not conflict.
///
/// Exclusion must hold between every holder that shares the backing storage: tasks and
/// threads in this process, other `Vault`s in this process, and other processes. A claimer
/// that can only see its own process must say so in its docs.
///
/// # Pairing with a `Store`
///
/// Some claimers lock the store's own storage (the filesystem claimer `flock`s the
/// directories of the filesystem store), so they only work with that store. Each claimer
/// documents which stores it may be used with; `Vault::new` doesn't check.
pub trait Claimer: Send + Sync + 'static {
    /// Tries to take an exclusive lock on `key` and everything beneath it.
    ///
    /// Returns the held lock on success. The lock stays held until it is released with
    /// [`ClaimLock::release`].
    ///
    /// # Conflicts
    ///
    /// Doesn't wait: if a conflicting claim is held (see [Exclusion](Claimer#exclusion)),
    /// the future completes with an error right away. Callers that want to wait retry with
    /// their own backoff and timeout.
    ///
    /// # Cancellation
    ///
    /// If the future is dropped before it completes, anything partially acquired must be
    /// released. A lock is never left held without a returned `ClaimLock`.
    ///
    /// # Errors
    ///
    /// Fails if a conflicting claim is held, or if the backend can't be reached or refuses
    /// the request (permissions, I/O).
    fn try_lock<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>>;
}
