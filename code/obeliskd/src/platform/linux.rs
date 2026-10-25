use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::{PlatformImpl, unix};
use crate::ObeliskdError;
use crate::control::Peer;

pub struct Linux;

impl PlatformImpl for Linux {
    type ControlListener = UnixListener;

    fn name() -> &'static str {
        "linux"
    }

    fn state_dir() -> PathBuf {
        PathBuf::from("/var/lib/obeliskd")
    }

    fn default_dns_address() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 79), 53)
    }

    fn control_socket_path() -> PathBuf {
        PathBuf::from("/run/obeliskd/obeliskd.sock")
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
