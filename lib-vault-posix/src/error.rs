use lib_vault::Key;
use std::io;
use std::path::PathBuf;

/// Errors specific to the filesystem backend. Construction returns them directly; store and
/// claim operations return them wrapped in `lib_vault::Error::StoreError` or
/// `lib_vault::Error::ClaimError`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PosixError {
    /// The vault root can't be used: not a directory, a symlink, a system directory, or its
    /// owner, group or mode doesn't match the sharing mode.
    #[error("invalid vault root {path}: {reason}")]
    InvalidRoot { path: PathBuf, reason: String },

    /// An ancestor of the vault root failed the sshd-style `strict_modes` check (D64).
    #[error("strict modes: {path}: {reason}")]
    StrictModes { path: PathBuf, reason: String },

    /// The vault root has extended ACLs, which aren't supported (D62, D70).
    #[error("vault root {path} has extended ACLs, which aren't supported")]
    ExtendedAcl { path: PathBuf },

    /// A previous write to this object was interrupted and left its journal behind (D63).
    /// Resolve it by hand: copy a complete journal over the object, or delete it.
    #[error("interrupted write to {key}: journal {journal} must be resolved by hand")]
    InterruptedWrite { key: Key, journal: PathBuf },

    /// Creating `key` would collide by ASCII case with an existing object (D6).
    #[error("{key} differs only by case from the existing object {existing}")]
    CaseCollision { key: Key, existing: String },

    /// A path component is a symlink, special file, hard-linked file, mount point, or not a
    /// directory where one is required (D17).
    #[error("unsupported file at {path}: {reason}")]
    Unsupported { path: PathBuf, reason: &'static str },

    /// A lock file kept changing between opening and locking it (D66).
    #[error("{key}: lock file {path} changed {attempts} times while being locked")]
    VerifyRetriesExhausted {
        key: Key,
        path: PathBuf,
        attempts: u32,
    },

    /// An I/O error, with the path it happened on.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl PosixError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: impl Into<io::Error>) -> Self {
        PosixError::Io {
            path: path.into(),
            source: source.into(),
        }
    }

    pub(crate) fn into_store_error(self) -> lib_vault::Error {
        lib_vault::Error::StoreError(Box::new(self))
    }

    pub(crate) fn into_claim_error(self) -> lib_vault::Error {
        lib_vault::Error::ClaimError(Box::new(self))
    }
}
