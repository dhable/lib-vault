/// How the vault is shared between users (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// Owned by the effective uid: root `0700`, directories `0700`, files `0600`.
    Private,
    /// Shared through a group: root group-owned by this gid with mode `2770` (setgid),
    /// directories `2770`, files `0660`.
    Group(u32),
}

/// Options for [`FsStore::new`](crate::FsStore::new) (D66). Built from `Default` with builder
/// methods, so new options don't break callers.
#[derive(Clone, Debug)]
pub struct FsStoreOptions {
    pub(crate) sharing: Sharing,
    pub(crate) journaling: bool,
    pub(crate) strict_modes: bool,
    pub(crate) max_verify_retries: u32,
}

impl Default for FsStoreOptions {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            journaling: true,
            strict_modes: true,
            max_verify_retries: 3,
        }
    }
}

impl FsStoreOptions {
    /// The sharing mode. Default: [`Sharing::Private`].
    pub fn sharing(mut self, sharing: Sharing) -> Self {
        self.sharing = sharing;
        self
    }

    /// Write through a journal so a crash never leaves a partial object (D63). Default: on.
    pub fn journaling(mut self, on: bool) -> Self {
        self.journaling = on;
        self
    }

    /// Check the root's ancestors like sshd's `StrictModes` at construction (D64). Default: on.
    pub fn strict_modes(mut self, on: bool) -> Self {
        self.strict_modes = on;
        self
    }

    /// Retries per lock level when a lock file changes between opening and locking it.
    /// `0` fails on the first mismatch (D66). Default: 3.
    pub fn max_verify_retries(mut self, n: u32) -> Self {
        self.max_verify_retries = n;
        self
    }

    pub(crate) fn dir_mode(&self) -> u32 {
        match self.sharing {
            Sharing::Private => 0o700,
            Sharing::Group(_) => 0o2770,
        }
    }

    pub(crate) fn file_mode(&self) -> u32 {
        match self.sharing {
            Sharing::Private => 0o600,
            Sharing::Group(_) => 0o660,
        }
    }
}
