use std::io;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::{PlatformImpl, unix};
use crate::ObeliskdError;
use crate::control::Peer;

pub struct MacOs;

impl PlatformImpl for MacOs {
    type ControlListener = UnixListener;

    fn name() -> &'static str {
        "macos"
    }

    fn state_dir() -> PathBuf {
        PathBuf::from("/Library/Application Support/obeliskd")
    }

    fn control_socket_path() -> PathBuf {
        PathBuf::from("/var/run/obeliskd/obeliskd.sock")
    }

    fn bind_control(path: &Path) -> Result<UnixListener, ObeliskdError> {
        unix::bind_control(path)
    }

    fn unbind_control(path: &Path) {
        unix::unbind_control(path);
    }

    fn peer(io: &UnixStream) -> io::Result<Peer> {
        unix::peer(io)
    }

    fn control_client(path: &Path) -> Result<reqwest::Client, ObeliskdError> {
        unix::control_client(path)
    }

    fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
        unix::shutdown_signal()
    }
}
