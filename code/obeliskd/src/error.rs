/// Every error obeliskd can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ObeliskdError {
    #[error("{message}")]
    Generic { message: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
