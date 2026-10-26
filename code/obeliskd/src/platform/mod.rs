//! Everything that differs per OS.
//!
//! [`PlatformImpl`] is the contract; each OS implements it on a unit struct, and exactly one of
//! those is exported as [`Platform`], chosen at compile time. Call through the type
//! (`Platform::name()`), never `&dyn`: the calls are static and other OSes' code isn't compiled.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};

use ipnet::Ipv4Net;
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

    /// Prepares the state dir before anything is read from it: creates it (`0700`) if missing,
    /// makes everything the daemon creates from now on private (umask `077`), and checks that the
    /// dir and its config are owned by the daemon's user and not writable by anyone else
    /// (tightening them, with a warning, if they're merely readable by others).
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::StateDir`] if it can't be created, or fails the checks.
    fn prepare_state_dir(dir: &Path) -> Result<(), ObeliskdError>;

    /// The daemon's own (effective) user id. Callers with it are trusted like root, so a daemon
    /// run as yourself (in development) can be managed by yourself.
    fn current_uid() -> u32;

    /// Makes every address in a host subnet local (Linux: one `local` route; macOS: nothing, as
    /// addresses become local one alias at a time). Idempotent.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Os`] if the OS refuses.
    fn ensure_host_subnet(cidr: Ipv4Net) -> Result<(), ObeliskdError>;

    /// Undoes [`ensure_host_subnet`](Self::ensure_host_subnet). Already gone is fine.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Os`] if the OS refuses.
    fn remove_host_subnet(cidr: Ipv4Net) -> Result<(), ObeliskdError>;

    /// Claims a supernet's whole range so unallocated addresses fail fast instead of leaking out
    /// the default route (Linux: an `unreachable` route; macOS: nothing). Idempotent.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Os`] if the OS refuses.
    fn ensure_supernet_route(cidr: Ipv4Net) -> Result<(), ObeliskdError>;

    /// Undoes [`ensure_supernet_route`](Self::ensure_supernet_route). Already gone is fine.
    ///
    /// # Errors
    ///
    /// [`ObeliskdError::Os`] if the OS refuses.
    fn remove_supernet_route(cidr: Ipv4Net) -> Result<(), ObeliskdError>;

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
