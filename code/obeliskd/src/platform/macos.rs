use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
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

    fn default_dns_address() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 79), 53)
    }

    fn ensure_local_address(address: Ipv4Addr) -> Result<(), ObeliskdError> {
        if unix::is_local(address) {
            return Ok(());
        }
        let error = |message: String| ObeliskdError::LocalAddress { address, message };

        // Adding an address is an ioctl, which would need `unsafe`; ifconfig does it for us.
        let output = std::process::Command::new("/sbin/ifconfig")
            .args(["lo0", "alias", &address.to_string(), "up"])
            .output()
            .map_err(|e| error(format!("couldn't run ifconfig: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(error(format!(
                "`ifconfig lo0 alias {address}` failed (needs root): {}",
                stderr.trim()
            )));
        }
        if unix::is_local(address) {
            Ok(())
        } else {
            Err(error(
                "still not bindable after adding the lo0 alias".to_string(),
            ))
        }
    }

    fn prepare_state_dir(dir: &Path) -> Result<(), ObeliskdError> {
        unix::prepare_state_dir(dir)
    }

    fn current_uid() -> u32 {
        unix::current_uid()
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
