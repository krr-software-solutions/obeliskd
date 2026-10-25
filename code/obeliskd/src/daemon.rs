//! The daemon: DNS and the control API, side by side.

use std::net::SocketAddrV4;
use std::path::Path;

use jiff::Timestamp;

use crate::control::api::DaemonInfo;
use crate::{ObeliskdError, Platform, PlatformImpl, control, dns};

/// Runs the daemon until a shutdown signal: answers DNS on `dns_address`, and serves the control
/// API on the socket at `socket`.
///
/// # Errors
///
/// If either can't be bound, or the control server fails.
pub async fn run(socket: &Path, dns_address: SocketAddrV4) -> Result<(), ObeliskdError> {
    // DNS first: it's the one likely to fail (port 53 needs root; on macOS the address may need an
    // lo0 alias), and then there's no control socket to clean up.
    Platform::ensure_local_address(*dns_address.ip())?;
    let dns_socket = dns::bind(dns_address).await?;
    let listener = Platform::bind_control(socket)?;
    let daemon = DaemonInfo {
        started_at: Timestamp::now(),
        dns_address,
    };
    eprintln!("obeliskd: control socket at {}", socket.display());
    eprintln!("obeliskd: answering *.{} on {dns_address} (UDP)", dns::ZONE);

    // The control server stops gracefully on the signal; DNS stops when it does.
    let result = tokio::select! {
        result = control::server::serve(listener, daemon, Platform::shutdown_signal()) => result,
        () = dns::serve(dns_socket, *dns_address.ip()) => unreachable!("DNS serves until dropped"),
    };
    Platform::unbind_control(socket);
    eprintln!("obeliskd: stopped");
    result
}
