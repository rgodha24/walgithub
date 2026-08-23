//! Error types for the WAL crate.

use thiserror::Error;
use walgit_store::StoreError;

/// Coordination-layer error. Re-exported from `walgit_store::coord`.
pub use walgit_store::coord::CoordError;

/// WAL-level error.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("repository not found")]
    NotFound,
    #[error("repository already exists")]
    AlreadyExists,
    #[error("ref conflict on {name}: expected {expected}, got {actual}")]
    RefConflict {
        name: String,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Coord(#[from] CoordError),
    #[error(transparent)]
    Git(#[from] walgit_git::GitError),
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Input rejected by validation (settings, policy): the caller's fault, 4xx.
    #[error("rejected: {0}")]
    Invalid(String),
    #[error("retry exhausted after {attempts} attempts")]
    Retry { attempts: u32 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The live pack set does not fit this instance's cache (`cache.max_bytes`);
    /// refs-level operations still work, object access must go elsewhere
    /// (bundle-uri, a disk-backed backend).
    #[error(
        "repository pack set is {bytes} bytes, larger than this instance's cache limit ({max} bytes); clone via bundle-uri"
    )]
    TooLarge { bytes: u64, max: u64 },
}

/// Per-ref error within a publish result.
#[derive(Debug, Clone, Error)]
pub enum RefError {
    #[error("non-fast-forward")]
    NonFastForward,
    #[error("conflict: expected {expected}, got {actual}")]
    Conflict { expected: String, actual: String },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("ref missing")]
    Missing,
}

impl From<WalError> for RefError {
    fn from(e: WalError) -> Self {
        match e {
            WalError::RefConflict {
                expected, actual, ..
            } => RefError::Conflict { expected, actual },
            other => RefError::Rejected(other.to_string()),
        }
    }
}
