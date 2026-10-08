use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncWrite};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::{fmt, future};

// This is currently a placeholder type that is generic. After more of the API is defined
// and some of the implementation is done, we can turn this into more concrete types.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// A hierarchical name for an object in a `Store`, in unix path syntax: components separated
/// by `/`, e.g. `sessions/123/notes.md`.
///
/// Keys are always relative to some scope (a `Store`, or a `Claim`'s key) and are strictly
/// canonical, so each key has exactly one spelling:
/// - the empty string is the root key (the scope itself);
/// - no leading or trailing `/`, and no empty components (`a//b`);
/// - no `.` or `..` components, so a key can never name something outside its scope;
/// - no NUL bytes.
///
/// Cloning is cheap (an `Arc<str>` refcount bump), so implementations can clone a `&Key`
/// into a `'static` future instead of borrowing it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(Arc<str>);

/// Why a string was rejected by `Key::new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// Starts with `/`. Keys are always relative to their scope.
    LeadingSlash,
    /// Ends with `/`.
    TrailingSlash,
    /// Contains `//`.
    EmptyComponent,
    /// Contains a `.` or `..` component.
    DotComponent,
    /// Contains a NUL byte.
    Nul,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            KeyError::LeadingSlash => "key must not start with '/'",
            KeyError::TrailingSlash => "key must not end with '/'",
            KeyError::EmptyComponent => "key must not contain empty components ('//')",
            KeyError::DotComponent => "key must not contain '.' or '..' components",
            KeyError::Nul => "key must not contain NUL bytes",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for KeyError {}

impl Key {
    /// Parses `s` as a key, rejecting anything that isn't in canonical form.
    pub fn new(s: &str) -> Result<Self, KeyError> {
        Self::validate(s)?;
        Ok(Self(Arc::from(s)))
    }

    /// The root key: the whole scope it is applied to.
    pub fn root() -> Self {
        Self(Arc::from(""))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Iterates over the `/`-separated components. The root key has none.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    /// Appends `other` below `self`.
    ///
    /// Unlike `Path::join`, `other` can never replace `self`: keys are always relative, so
    /// the result is always `self` or something beneath it. `ScopedStore` relies on this to
    /// keep callers inside their scope. Joining with the root key on either side returns
    /// the other key unchanged.
    pub fn join(&self, other: &Key) -> Key {
        if self.is_root() {
            other.clone()
        } else if other.is_root() {
            self.clone()
        } else {
            Key(Arc::from(format!("{}/{}", self.0, other.0)))
        }
    }

    /// True if `base` is `self` or one of its ancestors. Compares whole components, so
    /// `a/bc` does not start with `a/b`. Every key starts with the root key.
    pub fn starts_with(&self, base: &Key) -> bool {
        self.strip_prefix_str(base).is_some()
    }

    /// Returns `self` relative to `base`, or `None` if `base` isn't `self` or an ancestor.
    /// `a/b/c` stripped of `a` is `b/c`; a key stripped of itself is the root key.
    pub fn strip_prefix(&self, base: &Key) -> Option<Key> {
        self.strip_prefix_str(base).map(|rest| Key(Arc::from(rest)))
    }

    /// The key one level up, or `None` for the root key. The parent of `a` is the root key.
    pub fn parent(&self) -> Option<Key> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rsplit_once('/') {
            Some((parent, _)) => Key(Arc::from(parent)),
            None => Key::root(),
        })
    }

    /// The last component, or `None` for the root key.
    pub fn object_name(&self) -> Option<&str> {
        self.components().next_back()
    }

    fn strip_prefix_str(&self, base: &Key) -> Option<&str> {
        if base.is_root() {
            return Some(&self.0);
        }
        match self.0.strip_prefix(&*base.0)? {
            "" => Some(""),
            rest => rest.strip_prefix('/'),
        }
    }

    fn validate(s: &str) -> Result<(), KeyError> {
        if s.is_empty() {
            return Ok(());
        }
        if s.contains('\0') {
            return Err(KeyError::Nul);
        }
        if s.starts_with('/') {
            return Err(KeyError::LeadingSlash);
        }
        if s.ends_with('/') {
            return Err(KeyError::TrailingSlash);
        }
        for component in s.split('/') {
            match component {
                "" => return Err(KeyError::EmptyComponent),
                "." | ".." => return Err(KeyError::DotComponent),
                _ => {}
            }
        }
        Ok(())
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Key {
    type Err = KeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Key::new(s)
    }
}

impl TryFrom<&str> for Key {
    type Error = KeyError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Key::new(s)
    }
}

impl TryFrom<String> for Key {
    type Error = KeyError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Key::validate(&s)?;
        Ok(Key(Arc::from(s)))
    }
}

/// Type definition that must be satisified for the object data reader
/// used by the Store implementation.
pub type ObjectReader<'a> = Pin<Box<dyn AsyncRead + Send + 'a>>;

/// Type definition that must be satisified for the object data writer
/// used by the Store implementation.
pub type ObjectWriter<'a> = Pin<Box<dyn AsyncWrite + Send + 'a>>;

/// A `Claim` provides exclusive access to objects stored at a key and any child keys. Holding
/// a `Claim` is the only way to list, delete, read and write objects.
///
/// `Claim` is safe to move between tasks, making it async compatable with runtimes like tokio
/// (e.g. `Claim` implements `Send`). Although it is not possible to use a shared `Claim` for
/// any purpose (e.g. `Claim` does not impelemtn `Sync`). This is done by requiring a mutable
/// reference to use any method on the `Claim`. It does not mean that every method changes the
/// state of the object.
pub struct Claim {
    store: Arc<dyn Store>,
    lock: Option<Box<dyn ClaimLock>>,
    state: Arc<Mutex<VaultState>>,
}

impl Claim {
    // Create a new claim instance with the `Store` keys scoped to `base`. Intended
    // to be used by `Vault` to create a claim after a claim's lock is obtained.
    fn new(store: Arc<dyn Store>, base: Key, state: Arc<Mutex<VaultState>>) -> Self {
        let store = Arc::new(ScopedStore::new(base, store));
        let lock = None;
        Self { store, lock, state }
    }

    // Helper method that allows setting the lock object after construction. We do this
    // so the state count of claim objects can run the drop logic even if the Claimer
    // fails to obtain a lock.
    fn set_lock(&mut self, lock: Box<dyn ClaimLock>) {
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
    ///    let list = claim.list(Key::new("parent")?)?;
    /// ```
    /// Would list all the objects that have keys that are children of `top/parent`.
    pub async fn list(&mut self, prefix: &Key) -> Result<Vec<Key>, Error> {
        self.store.list(prefix).await
    }

    /// Deletes a specific object stored at `base` + `key`. Calling this method with a
    /// key that does not exist in the store is considered a successful delete operation.
    /// This behavior matches the semantics of the HTTP DELETE method.
    pub async fn delete(&mut self, key: &Key) -> Result<(), Error> {
        self.store.delete(key).await
    }

    /// Opens a specific object at `base` + `key` for reading. The resulting reader is valid
    /// for the duration of the claim. Reading a key that does not point an object will return
    /// an error.
    pub async fn open_read<'a>(&'a mut self, key: &Key) -> Result<ObjectReader<'a>, Error> {
        self.store.open_read(key).await
    }

    /// Opens a specific object at `base` + `key` for writing. The specific beahviors around when
    /// data is committed/flushed and what happens with in flight requests after the Claim is
    /// released should be documented by the the `Store` implementation. Due to the mutable borrowing
    /// and the lifetimes, there can only be a single open reader or writer at a given time.
    ///
    /// Writing to a key pointing at an existing object should overwrite the exiting object data.
    /// Writing to a key that does not have an object will create the object with the given key.
    pub async fn open_write<'a>(&'a mut self, key: &Key) -> Result<ObjectWriter<'a>, Error> {
        self.store.open_write(key).await
    }

    /// Releases the underyling Claim lock issued when the Claim was created. This method exposes
    /// the release status and any error data. If a the Claim owner doesn't call this method, it
    /// will be called in `drop` and any errors will be logged with the tracing crate.
    pub fn release(&mut self) -> Result<(), Error> {
        if let Some(lock) = self.lock.take() {
            lock.release()?;
        }
        Ok(())
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = self
            .release()
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
/// the vault wraps the returned [`ClaimLock`] in a [`Claim`] scoped to the key. A `Claimer`
/// only locks; it never touches object data, which belongs to the [`Store`].
///
/// # Exclusion
///
/// A claim on `k` covers `k` and every key beneath it, compared component-wise
/// ([`Key::starts_with`]): a claim on `a/b` covers `a/b`, `a/b/c` and `a/b/c/d`, but not
/// `a/bc` or `a/z`. Two claims conflict when one covers the other's key, so while `a/b` is
/// held, `try_lock` must refuse all of:
/// - `a/b` itself;
/// - any descendant, such as `a/b/c`;
/// - any ancestor, such as `a` or the root key `""`.
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
    // REVIEW(nit): the link below to `ClaimLock::try_release` is dead (`cargo doc` warns), and
    // the `futures` `executor` feature in Cargo.toml is now unused (nothing calls `block_on`).
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

/// Interface required for any strategy used for storing the actual object data. Our first
/// implementation will be a file system but it could also be a database, object store or
/// in memory.
///
/// Note for implementors: the returned futures borrow `self` (`'a`) but not `key`. Clone the
/// key (cheap: it's an `Arc<str>`) into the future rather than borrowing it.
pub trait Store: Send + Sync + 'static {
    /// Returns the keys of all objects beneath `key` (component-wise), relative to `key`.
    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>>;

    /// Deletes the object at `key`. Succeeds if there is no such object.
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>>;

    /// Opens the object at `key` for reading. Errors if there is no such object.
    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>>;

    /// Opens the object at `key` for writing, creating or overwriting it. Implementations
    /// document when written data is committed (e.g. on `close()`).
    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>>;
}

/// Wraps a Store and prepends a defined key automatically
pub struct ScopedStore {
    base: Key,
    inner: Arc<dyn Store>,
}

impl ScopedStore {
    /// Wraps `inner` so every key is resolved beneath `base`. Since `Key::join` can only
    /// nest, callers of the scoped store can't reach anything outside `base`.
    pub fn new(base: Key, inner: Arc<dyn Store>) -> Self {
        Self { base, inner }
    }
}

impl Store for ScopedStore {
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        self.inner.delete(&self.base.join(key))
    }

    fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
        self.inner.list(&self.base.join(key))
    }

    fn open_read<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
        self.inner.open_read(&self.base.join(key))
    }

    fn open_write<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
        self.inner.open_write(&self.base.join(key))
    }
}

/// Internal helper type to hold the shutdown state and outstanding claim counts
/// in order to implement the Vault shutdown behavior.
#[derive(Default)]
struct VaultState {
    shutting_down: bool,
    outstanding_claims: usize,
    waiters: Vec<Waker>,
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
                return Err("todo return better error type".into());
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

    mod playground {
        use std::panic;

        use futures::AsyncReadExt;

        use super::*;

        struct MemStore;
        impl Store for MemStore {
            fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
                todo!()
            }
            fn list(&self, key: &Key) -> BoxFuture<'_, Result<Vec<Key>, Error>> {
                todo!()
            }
            fn open_read<'a>(
                &'a self,
                key: &Key,
            ) -> BoxFuture<'a, Result<ObjectReader<'a>, Error>> {
                todo!()
            }
            fn open_write<'a>(
                &'a self,
                key: &Key,
            ) -> BoxFuture<'a, Result<ObjectWriter<'a>, Error>> {
                todo!()
            }
        }

        struct MemClaimer;
        impl Claimer for MemClaimer {
            fn try_lock<'a>(
                &'a self,
                key: &Key,
            ) -> BoxFuture<'a, Result<Box<dyn ClaimLock>, Error>> {
                todo!()
            }
        }

        async fn example_usage() {
            let v = Vault::new(MemStore, MemClaimer);
            let k = Key::new("foo").unwrap();
            let mut c = v.claim(k).await.unwrap();

            let k2 = Key::new("bar").unwrap();
            let mut reader = c.open_read(&k2).await.unwrap();

            let mut buf: Vec<u8> = Vec::new();
            match reader.read_to_end(&mut buf).await {
                Err(_) => panic!("error"),
                Ok(_size) => { /* do something with buf */ }
            }

            drop(reader);
            drop(c);
        }
    }

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
        }
    }

    mod key_tests {
        use super::*;

        fn key(s: &str) -> Key {
            Key::new(s).unwrap()
        }

        #[test]
        fn accepts_canonical_keys() {
            for s in [
                "",
                "a",
                "a/b",
                "sessions/123/notes.md",
                ".hidden",
                "a/..b",
                "a b/c",
            ] {
                assert_eq!(Key::new(s).unwrap().as_str(), s);
            }
        }

        #[test]
        fn rejects_non_canonical_keys() {
            let cases = [
                ("/", KeyError::LeadingSlash),
                ("/a", KeyError::LeadingSlash),
                ("a/", KeyError::TrailingSlash),
                ("a//b", KeyError::EmptyComponent),
                (".", KeyError::DotComponent),
                ("..", KeyError::DotComponent),
                ("a/./b", KeyError::DotComponent),
                ("a/../b", KeyError::DotComponent),
                ("a/..", KeyError::DotComponent),
                ("a\0b", KeyError::Nul),
            ];
            for (s, err) in cases {
                assert_eq!(Key::new(s), Err(err), "{s:?}");
                assert_eq!(Key::try_from(s.to_string()), Err(err), "{s:?}");
            }
        }

        #[test]
        fn root() {
            assert!(Key::root().is_root());
            assert_eq!(Key::root(), key(""));
            assert_eq!(Key::root().components().count(), 0);
            assert_eq!(Key::root().parent(), None);
            assert_eq!(Key::root().object_name(), None);
        }

        #[test]
        fn join() {
            assert_eq!(key("a").join(&key("b/c")), key("a/b/c"));
            assert_eq!(key("a/b").join(&key("c")), key("a/b/c"));
            assert_eq!(Key::root().join(&key("a")), key("a"));
            assert_eq!(key("a").join(&Key::root()), key("a"));
            assert_eq!(Key::root().join(&Key::root()), Key::root());

            let (a, b, c) = (key("a"), key("b/x"), key("c"));
            assert_eq!(a.join(&b).join(&c), a.join(&b.join(&c)));
        }

        #[test]
        fn join_always_nests() {
            let base = key("scope");
            for other in ["", "a", "a/b", "scope", "x/scope"] {
                assert!(base.join(&key(other)).starts_with(&base), "{other:?}");
            }
        }

        #[test]
        fn components() {
            let k = key("a/b/c");
            let parts: Vec<_> = k.components().collect();
            assert_eq!(parts, ["a", "b", "c"]);
        }

        #[test]
        fn starts_with_compares_components() {
            assert!(key("a/b/c").starts_with(&key("a/b")));
            assert!(key("a/b").starts_with(&key("a/b")));
            assert!(key("a").starts_with(&Key::root()));
            assert!(!key("a/bc").starts_with(&key("a/b")));
            assert!(!key("a").starts_with(&key("a/b")));
            assert!(!Key::root().starts_with(&key("a")));
        }

        #[test]
        fn strip_prefix() {
            assert_eq!(key("a/b/c").strip_prefix(&key("a")), Some(key("b/c")));
            assert_eq!(key("a/b").strip_prefix(&key("a/b")), Some(Key::root()));
            assert_eq!(key("a/b").strip_prefix(&Key::root()), Some(key("a/b")));
            assert_eq!(key("a/bc").strip_prefix(&key("a/b")), None);
        }

        #[test]
        fn parent_and_object_name() {
            assert_eq!(key("a/b/c").parent(), Some(key("a/b")));
            assert_eq!(key("a").parent(), Some(Key::root()));
            assert_eq!(key("a/b/c").object_name(), Some("c"));
            assert_eq!(key("a").object_name(), Some("a"));
        }

        #[test]
        fn parse_and_display_round_trip() {
            let k: Key = "a/b".parse().unwrap();
            assert_eq!(k.to_string(), "a/b");
            assert_eq!(Key::try_from("a/b").unwrap(), k);
            assert_eq!(k.as_ref(), "a/b");
        }
    }
}
