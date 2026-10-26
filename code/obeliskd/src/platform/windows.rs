// Windows isn't supported yet. This file is only compiled when targeting Windows (see `mod.rs`),
// so this fails exactly those builds. Delete the line below to start supporting it.
compile_error!("obeliskd doesn't support Windows yet");

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};

use tokio::net::{TcpListener, TcpStream};

use super::PlatformImpl;
use crate::ObeliskdError;
use crate::control::Peer;

pub struct Windows;

// The control socket will be a named pipe (`\\.\pipe\obeliskd`): reqwest can already connect to
// one (`ClientBuilder::windows_named_pipe`), but the server side and the caller's identity (the
// client's token / SID) are still to do. `TcpListener` is a stand-in so this compiles.
impl PlatformImpl for Windows {
    type ControlListener = TcpListener;

    fn name() -> &'static str {
        "windows"
    }

    fn state_dir() -> PathBuf {
        let program_data =
            std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        PathBuf::from(program_data).join("obeliskd")
    }

    fn default_dns_address() -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 79), 53)
    }

    fn ensure_local_address(address: Ipv4Addr) -> Result<(), ObeliskdError> {
        // All of 127.0.0.0/8 is local on Windows; adding other addresses is still to do.
        if UdpSocket::bind((address, 0)).is_ok() {
            Ok(())
        } else {
            Err(ObeliskdError::LocalAddress {
                address,
                message: "adding addresses isn't supported on Windows yet".to_string(),
            })
        }
    }

    fn prepare_state_dir(_dir: &Path) -> Result<(), ObeliskdError> {
        todo!("create the state dir with an owner-only ACL")
    }

    fn current_uid() -> u32 {
        todo!("Windows has SIDs, not uids")
    }

    fn control_socket_path() -> PathBuf {
        PathBuf::from(r"\\.\pipe\obeliskd")
    }

    fn bind_control(_path: &Path) -> Result<TcpListener, ObeliskdError> {
        todo!("bind a named pipe")
    }

    fn unbind_control(_path: &Path) {}

    fn peer(_io: &TcpStream) -> io::Result<Peer> {
        todo!("identify a named-pipe client")
    }

    fn control_client(_path: &Path) -> Result<reqwest::Client, ObeliskdError> {
        todo!("reqwest's windows_named_pipe")
    }

    async fn shutdown_signal() {
        let _ = tokio::signal::ctrl_c().await;
    }
}
