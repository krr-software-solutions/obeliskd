use std::path::PathBuf;

/// Every error obeliskd can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ObeliskdError {
    #[error("{message}")]
    Generic { message: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("another obeliskd is already listening on {}", path.display())]
    AlreadyRunning { path: PathBuf },

    #[error("failed to bind the control socket {}: {source}", path.display())]
    ControlBind {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("control server failed: {0}")]
    ControlServe(std::io::Error),

    #[error("failed to set up the control client: {0}")]
    ControlClient(reqwest::Error),

    #[error("can't reach the daemon at {} (is it running?): {source}", socket.display())]
    ControlConnect {
        socket: PathBuf,
        source: reqwest::Error,
    },

    #[error("the daemon returned {status}: {message}")]
    ControlStatus { status: u16, message: String },

    #[error("bad response from the daemon: {0}")]
    ControlDecode(reqwest::Error),
}
