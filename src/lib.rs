use futures::future::BoxFuture;
use futures::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// This is currently a placeholder type that is generic. After more of the API is defined
// and some of the implementation is done, we can turn this into more concrete types.
type Error = Box<dyn std::error::Error>;

// This need to be flushed out more. It should work similar to the fs::Path in that
// it's a hiearchy scoped, string repr of a way to get into a key. Should follow
// newtype patterns where possible.
pub struct Key; // TODO: flush this out more
impl Key {
    pub fn join(&self, other: &Key) -> &Self {
        todo!("implement with signature similar to Path::join")
    }
}

// Convenience type for defining generic async return types in the API layer. We do this
// to ensure that support any async runtime can be implemented in the future.
// pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub struct Claim {
    store: Arc<dyn Store>,
}

// Claim instances are Send so ownership of the Claim can be transfered to
// another thread.
// They are not Sync in that a Claim refernce cannot be shared across threads.
// This is done to ensure that there's only a single owner of a given Claim or
// any of its subclaims.
#[negative_impl::negative_impl]
impl !Sync for Claim {}
unsafe impl Send for Claim {}

impl Claim {
    // This is not pub since it should only be used by the Vault code
    fn new(store: Arc<dyn Store>, _key: &Key) -> Self {
        Self { store }
    }

    // what the user will use to list contents scoped to the key and children
    pub fn list(&self, key: &Key) -> BoxFuture<'_, Result<&'_ [String], Error>> {
        self.store.list(key)
    }

    // what the user will use to delete an object
    pub fn delete(&self, key: &Key) -> BoxFuture<'_, Result<(), Error>> {
        self.store.delete(key)
    }

    // what the user will use to open an object for CRUD
    pub fn open(&self, key: &Key) -> BoxFuture<'_, Result<Box<dyn StoreObject>, Error>> {
        self.store.open(key)
    }

    /// Invalidates this Claim, which allows other call sites to claim
    /// the same resource. Consumes the ownership of the Claim, which then
    /// also renders the Dir and File references invalid at compile time.
    #[must_use]
    fn release_impl(&mut self) -> Result<(), Error> {
        todo!()
    }

    #[must_use]
    pub fn release(mut self) -> Result<(), Error> {
        self.release_impl()
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = self.release_impl();
    }
}

// ------------------------

/// Interface required for any strategy for obtaining a claim to an object in
/// the vault.
pub trait Claimer: Send + Sync + 'static {
    // Does the strategy specific logic to create a Claim that is bound to the store
    // and scoped to key and its children. Returns an error if it can't obtain the
    // exclusive claim object
    fn try_claim<'a>(
        &'a self,
        store: Arc<dyn Store>,
        key: &Key,
    ) -> BoxFuture<'a, Result<Claim, Error>>;
}

pub trait StoreObject: AsyncRead + AsyncSeek + AsyncWrite {}

// Interface required for any strategy used for storing the actual object data. Our first
// implementation will be a file system but it could also be a database, object store or
// in memroy.
pub trait Store: Send + Sync + 'static {
    /// Given a path, returns all of the objects that are children of the path in
    /// a deep search. For file systems, this would be a recursive listing of all
    /// the file paths. For other storage systems, path would be considered a matching
    /// prefix.
    fn list<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<&'a [String], Error>>;

    /// Given a path to an item stored, deletes the object. Returns Ok() if there is
    /// no object referenced by path.
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>>;

    /// Given a path to an item stored, return a type that can read, write and seek
    /// on the data for reading, writing, etc.
    fn open<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn StoreObject>, Error>>;
}

/// Wraps a Store and prepends a defined key automatically
pub struct ScopedStore {
    base: Key,
    inner: Arc<dyn Store>,
}

impl ScopedStore {
    // todo: impement new
}

impl Store for ScopedStore {
    fn delete<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<(), Error>> {
        self.inner.delete(self.base.join(key))
    }

    fn list<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<&'a [String], Error>> {
        self.inner.list(self.base.join(key))
    }

    fn open<'a>(&'a self, key: &Key) -> BoxFuture<'a, Result<Box<dyn StoreObject>, Error>> {
        self.inner.open(self.base.join(key))
    }
}

// Handle to the thing the code will need.
#[derive(Clone)]
pub struct Vault {
    store: Arc<dyn Store>,
    claimer: Arc<dyn Claimer>,
    shutting_down: Arc<AtomicBool>,
}

impl Vault {
    /// Creates a new Vault given an implementation of a storage engine `store` and
    /// a claim locking engine `claimer`.
    pub fn new(store: impl Store, claimer: impl Claimer) -> Self {
        Self {
            store: Arc::new(store),
            claimer: Arc::new(claimer),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Attempt to obtain a new claim from this vault's `Claimer` engine for a given
    /// obhject path asychronously. This can result in either a valid `Claim` object, which
    /// provides exclusive access to the path and it's subpaths. Can result in an error
    /// if a claim cannot be obtained.
    pub fn claim(&self, key: &Key) -> BoxFuture<'_, Result<Claim, Error>> {
        if !self.shutting_down.load(Ordering::Relaxed) {
            return self.claimer.try_claim(self.store.clone(), key);
        }
        todo!("return Err because we're shutting down")
    }

    /// Blocks until all outstanding claims have been released. Will prevent new Claims
    /// from being issued.
    pub async fn shutdown(self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        todo!("wait for all Claims to have been released")
    }
}
