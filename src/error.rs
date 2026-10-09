use crate::{Key, KeyError};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("vault is shutting down")]
    ShuttingDown,

    #[error("invalid key")]
    Key(#[from] KeyError),

    #[error("{key} is already held by another claim")]
    AlreadyClaimed { key: Key },

    #[error("no object at {key}")]
    NotFound { key: Key },

    #[error("error obtaining a claim")]
    ClaimError(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("vault store error")]
    StoreError(#[source] Box<dyn std::error::Error + Send + Sync>),
}
