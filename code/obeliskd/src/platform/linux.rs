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

    fn ensure_local_address(address: Ipv4Addr) -> Result<(), ObeliskdError> {
        // All of 127.0.0.0/8 is local on Linux; anything else isn't added by obeliskd (yet).
        if unix::is_local(address) {
            Ok(())
        } else {
            Err(ObeliskdError::LocalAddress {
                address,
                message: "adding addresses isn't supported on Linux yet; use one in 127.0.0.0/8"
                    .to_string(),
            })
        }
    }

    fn prepare_state_dir(dir: &Path) -> Result<(), ObeliskdError> {
        unix::prepare_state_dir(dir)
    }

    fn current_uid() -> u32 {
        unix::current_uid()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_of_127_8_is_already_local() {
        Linux::ensure_local_address(Ipv4Addr::new(127, 0, 0, 79)).unwrap();
        Linux::ensure_local_address(Ipv4Addr::new(127, 200, 3, 4)).unwrap();
    }

    #[test]
    fn non_loopback_addresses_are_not_added() {
        let error = Linux::ensure_local_address(Ipv4Addr::new(192, 0, 2, 1)).unwrap_err();
        assert!(matches!(error, ObeliskdError::LocalAddress { .. }));
    }
}
