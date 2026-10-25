//! Everything that differs per OS.
//!
//! [`PlatformImpl`] is the contract; each OS implements it on a unit struct, and exactly one of
//! those is exported as [`Platform`], chosen at compile time. Call through the type
//! (`Platform::name()`), never `&dyn`: the calls are static and other OSes' code isn't compiled.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use axum::serve::Listener;

use crate::ObeliskdError;
use crate::control::Peer;

/// What each OS has to provide.
///
/// Only the OS-specific edges live here: paths, the control socket's transport, and who is on the
/// other end of it. Everything above that (the API, auth, state) is shared code.
pub trait PlatformImpl {
    /// What the control server accepts connections on.
    type ControlListener: Listener;

    /// The OS's name, e.g. `"linux"`.
    fn name() -> &'static str;

    /// Where the daemon keeps everything that persists: settings, rules, the CA, certs.
    ///
    /// The daemon owns it; nothing in it is edited by hand.
    fn state_dir() -> PathBuf;

    /// Where the daemon answers DNS, unless told otherwise.
    fn default_dns_address() -> SocketAddrV4;

    /// Makes `address` local to this host, so it can be bound. Does nothing if it already is.
    ///
    /// On Linux all of `127.0.0.0/8` is local already. macOS only has `127.0.0.1` on `lo0`, so any
    /// other address is added as an `lo0` alias (needs root; gone after a reboot).
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::LocalAddress`] if it isn't local and can't be made so.
    fn ensure_local_address(address: Ipv4Addr) -> Result<(), ObeliskdError>;

    /// Where the daemon listens for control connections, unless told otherwise.
    fn control_socket_path() -> PathBuf;

    /// Binds the control socket at `path`, replacing a stale one.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::AlreadyRunning`] if a daemon is already listening there, or
    /// [`ObeliskdError::ControlBind`].
    fn bind_control(path: &Path) -> Result<Self::ControlListener, ObeliskdError>;

    /// Cleans up after [`bind_control`](Self::bind_control), once the server has stopped.
    fn unbind_control(path: &Path);

    /// Who is on the other end of an accepted control connection.
    ///
    /// # Errors
    ///
    /// If the OS won't say.
    fn peer(io: &<Self::ControlListener as Listener>::Io) -> io::Result<Peer>;

    /// An HTTP client whose every request goes to the control socket at `path`.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::ControlClient`] if the client can't be built.
    fn control_client(path: &Path) -> Result<reqwest::Client, ObeliskdError>;

    /// Resolves when the daemon is asked to stop (Ctrl-C, and the service manager's signal).
    fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static;
}

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Linux as Platform;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOs as Platform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::Windows as Platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("obeliskd supports Linux, macOS, and Windows only");
