use crate::DispatchError;
use lib_vault::{Error, Key};

/// Errors specific to the SQLite backend. They reach callers wrapped in
/// `lib_vault::Error::StoreError` or `Error::ClaimError`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SqliteError {
    #[error("sqlite error")]
    Sql(#[from] sqlx::Error),

    #[error("dispatcher error")]
    Dispatch(#[source] DispatchError),

    #[error("the value at {key} isn't the requested type")]
    WrongType { key: Key },

    #[error("the sqlite claimer has been shut down")]
    ClaimerShutDown,
}

// Wraps a backend error for the store side of the `lib-vault` API.
pub(crate) fn store_error(err: impl Into<SqliteError>) -> Error {
    Error::StoreError(Box::new(err.into()))
}

// Wraps a dispatcher error for the store side of the `lib-vault` API.
pub(crate) fn dispatch_error(err: DispatchError) -> Error {
    store_error(SqliteError::Dispatch(err))
}

// Wraps a backend error for the claimer side of the `lib-vault` API.
pub(crate) fn claim_error(err: impl Into<SqliteError>) -> Error {
    Error::ClaimError(Box::new(err.into()))
}
