//! Error type of the library.

use std::io;

/// Every error that the library returns to its callers.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A device lookup found zero or many devices.
    #[error("{0}")]
    Lookup(String),
    /// A broker or the QUBO cloud cannot be reached, or it refused us.
    #[error("{0}")]
    Connection(String),
    /// The socket safety interlock refused a switch-off.
    #[error("{0}")]
    Interlock(String),
    /// A value is out of range.
    #[error("{0}")]
    Value(String),
    /// Neither the local broker nor a cloud login is available.
    #[error("{0}")]
    NoPath(String),
    /// The QUBO account service refused a request, or a login is necessary.
    #[error("{0}")]
    Auth(String),
    /// A file or process operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A JSON document is not valid.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Result type of the library.
pub type Result<T> = std::result::Result<T, Error>;
