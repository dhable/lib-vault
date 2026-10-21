use crate::DispatchError;
use lib_vault::Key;

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
}
