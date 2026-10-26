use std::net::{Ipv4Addr, SocketAddrV4};
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

    #[error("{address} isn't a local address, and couldn't be made one: {message}")]
    LocalAddress { address: Ipv4Addr, message: String },

    #[error("failed to bind DNS on {address}: {source}{}", bind_hint(source))]
    DnsBind {
        address: SocketAddrV4,
        source: std::io::Error,
    },

    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    Conflict(String),

    #[error("state dir {}: {message}", path.display())]
    StateDir { path: PathBuf, message: String },

    #[error("failed to read {}: {source}", path.display())]
    StateRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse {} (refusing to start rather than overwrite it): {source}", path.display())]
    StateParse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("failed to write {}: {source}", path.display())]
    StateWrite {
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

fn bind_hint(source: &std::io::Error) -> &'static str {
    match source.kind() {
        std::io::ErrorKind::PermissionDenied => {
            " (ports below 1024 need root; for development, try --dns-address 127.0.0.79:5379)"
        }
        _ => "",
    }
}
