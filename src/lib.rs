use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// Re-export of the File type so that it keeps parity with the
// Dir type below.
pub use std::fs::File;

// Implementation that matches std::fs::Dir but in a way that does
// not require nightly builds or special feature flags. When std::fs::Dir
// becomes stable, then this can become a re-export like File above.
pub struct Dir {}
impl Dir {
    // TODO: implement the stable methods
}

// This is currently a placeholder type that is generic. After more of the API is defined
// and some of the implementation is done, we can turn this into more concrete types.
type Error = Box<dyn std::error::Error>;

// Claim should either be a File or a Dir?
pub struct Claim<'a> {
    _vault: Arc<VaultInner>,
    _marker: PhantomData<&'a mut ()>,
}

// The Vault is now Send so it can be moved across threads but a Claim has to be used locally.
// Instead of sending a Claim, clone the Vault and then use `try_claim` on the local thread.
//
#[negative_impl::negative_impl]
impl<'a> !Send for Claim<'a> {}

impl<'a> Claim<'a> {
    /// Create another Claim scoped to this claim or it's children. The returned
    /// claim assumes the lifetime of the parent. The mutable borrow ensures that
    /// the parent cannot be used while there is an outstanding subclaim.
    #[must_use]
    pub fn sub_claim<'b, P: AsRef<Path>>(&'b mut self, path: P) -> Result<Claim<'b>, Error> {
        Ok(Claim {
            // additional stuff
            _vault: self._vault.clone(),
            _marker: PhantomData,
        })
    }

    /// Reference to the claim object as a file. Returns an Err if the path doesn't
    /// point to a File object.
    pub fn open(&self) -> Result<&File, Error> {
        // TODO this should return the VaultFile trait
        todo!("returns std::fs::File or tokio::fs::File depending on if this is async or not")
    }

    /// Reference to the claim object as a directory. Returns an Err if the path
    /// doesn't point to a Dir object.
    pub fn open_dir(&self) -> Result<&Dir, Error> {
        todo!()
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

impl<'a> Drop for Claim<'a> {
    fn drop(&mut self) {
        let _ = self.release_impl();
    }
}

struct VaultInner {}
impl VaultInner {
    /// Creates and initializes a new Vault based on the `root` path. This needs to take
    /// ownership of the directory at the OS level such that another Vault cannot be opened
    /// with the same root.
    fn new<P: AsRef<Path>>(root: P) -> Result<Self, Error> {
        todo!()
    }

    /// Attempts to lock either a directory (and conversely subdirectories) or
    /// a single file, specified based on the `path`. Either returns the
    /// Claim if it was able to establish it or returns why it can't. Does not
    /// block and returns an appropriate `Err` if the `Claim` can't be obtained.
    fn try_claim<P: AsRef<Path>>(&self, path: P) -> Result<Claim<'static>, Error> {
        todo!()
    }

    /// Disable issuing new Claims/Subclaims so that any shutdown logic can proceed with
    /// some assurances that the file system is shutdown in a stable state. Blocks the calling
    /// thread until all of the outstanding claims have been released.
    fn shutdown_impl(&mut self) -> Result<(), Error> {
        todo!()
    }
}

#[derive(Clone)]
pub struct Vault(Arc<VaultInner>);
impl Vault {
    /// Creates and initializes a new Vault based on the `root` path.
    pub fn new<P: AsRef<Path>>(root: P) -> Result<Self, Error> {
        VaultInner::new(root).map(|v| Self(Arc::new(v)))
    }

    /// Attempts to lock either a directory (and conversely subdirectories) or
    /// a single file, specified based on the `path`. Either returns the
    /// Claim if it was able to establish it or returns why it can't. Does not
    /// block and returns an appropriate `Err` if the `Claim` can't be obtained.
    pub fn try_claim<P: AsRef<Path>>(&self, path: P) -> Result<Claim<'static>, Error> {
        self.0.try_claim(path)
    }

    /// Attempts to lock either a directory (and conversely subdirectories) or
    /// a single file, specified based on the `path`. Blocks the caller
    /// until the `Claim` can be obtained or until timeout has elapsed.
    pub fn claim<P: AsRef<Path>>(
        &self,
        path: P,
        timeout: Duration,
    ) -> Result<Claim<'static>, Error> {
        todo!("implement using VaultInner::try_claim")
    }

    pub fn shutdown(self) {
        let mut inner = self.0;
        let _ = Arc::get_mut(&mut inner).map(VaultInner::shutdown_impl);
    }
}

mod test {
    use super::*;
    use std::io::Write;

    fn init_scratchpad_dir(dir: &Dir) {
        todo!()
    }

    fn init_artifact_dir(dir: &Dir) {
        todo!()
    }

    #[test]
    fn basic_usage() -> Result<(), Error> {
        let vault = Vault::new("~/vault")?;
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut session1 = vault.try_claim("session_123/agent_abc").unwrap();

                let scratch_dir_claim = session1.sub_claim("scratchpad").unwrap();
                let d1 = scratch_dir_claim.open_dir().unwrap();
                init_scratchpad_dir(&d1);
                scratch_dir_claim.release().unwrap();
                // init_scratchpad_dir(&mut d1);  // this should fail to compile (and it does!)

                let artifact_dir_claim = session1.sub_claim("artifacts").unwrap();
                let d2 = artifact_dir_claim.open_dir().unwrap();
                init_artifact_dir(&d2);
                artifact_dir_claim.release().unwrap();
                // init_artifact_dir(&d2); // this should fail to compile (and it does!)
            });

            s.spawn(|| {
                let mut session2 = vault.try_claim("session_456/config.json").unwrap();

                let config_file_claim = session2.sub_claim("config.json").unwrap();
                let mut f = config_file_claim.open().unwrap();
                write!(f, r#"{{}}"#).unwrap();

                drop(config_file_claim);
                // write!(f, r#"{{}}"#).unwrap(); // this should probably not compile (and it doesn't!)
            });
        });

        Ok(())
    }
}
