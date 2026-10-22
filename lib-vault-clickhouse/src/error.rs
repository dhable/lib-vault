use crate::DispatchError;
use lib_vault::{Error, Key};

/// Errors specific to the ClickHouse backend. They reach callers wrapped in
/// `lib_vault::Error::StoreError` or `Error::ClaimError`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClickhouseError {
    #[error("clickhouse error")]
    Client(#[from] clickhouse::error::Error),

    #[error("dispatcher error")]
    Dispatch(#[source] DispatchError),

    #[error("the value at {key} isn't the requested type")]
    WrongType { key: Key },

    #[error("the clickhouse claimer has been shut down")]
    ClaimerShutDown,

    /// Concurrent updates to the same claim rows kept invalidating this claim's writes. Not
    /// a conflict with another claim; retrying later can succeed.
    #[error("gave up claiming {key} after {attempts} attempts lost to concurrent updates")]
    Contended { key: Key, attempts: u32 },

    #[error("invalid claimer option: {0}")]
    InvalidOption(String),
}

// Wraps a backend error for the store side of the `lib-vault` API.
pub(crate) fn store_error(err: impl Into<ClickhouseError>) -> Error {
    Error::StoreError(Box::new(err.into()))
}

// Wraps a dispatcher error for the store side of the `lib-vault` API.
pub(crate) fn dispatch_error(err: DispatchError) -> Error {
    store_error(ClickhouseError::Dispatch(err))
}

// Wraps a backend error for the claimer side of the `lib-vault` API.
pub(crate) fn claim_error(err: impl Into<ClickhouseError>) -> Error {
    Error::ClaimError(Box::new(err.into()))
}
