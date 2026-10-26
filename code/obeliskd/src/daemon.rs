//! The daemon: DNS and the control API, side by side.

use std::net::SocketAddrV4;
use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use tracing::{info, warn};

use crate::control::api::DaemonInfo;
use crate::control::server::AppState;
use crate::manage::DnsRouting;
use crate::os::SystemOs;
use crate::state::Store;
use crate::users::SystemUsers;
use crate::{ObeliskdError, Platform, PlatformImpl, control, dns};

/// Runs the daemon until a shutdown signal: answers DNS on `dns_address`, serves the control API
/// on the socket at `socket`, and keeps its state in `state_dir`.
///
/// # Errors
///
/// If either can't be bound, or the control server fails.
pub async fn run(
    socket: &Path,
    dns_address: SocketAddrV4,
    state_dir: &Path,
) -> Result<(), ObeliskdError> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        uid = Platform::current_uid(),
        "starting obeliskd"
    );
    // Everything below changes routes and docker networks.
    if Platform::current_uid() != 0 {
        return Err(ObeliskdError::NeedsRoot);
    }
    // State next: nothing should start serving on top of state that can't be trusted or read.
    Platform::prepare_state_dir(state_dir)?;
    let store = Arc::new(Store::load(state_dir, Arc::new(SystemOs))?);
    let counts = store.counts().await;
    info!(
        dir = %state_dir.display(),
        supernets = counts.supernets,
        subnets = counts.subnets,
        ips = counts.ips,
        hosts = counts.hosts,
        "loaded state"
    );
    // Routes don't survive a reboot, and docker networks may have drifted: make the OS match
    // the config again. Failures are reported, not fatal: DNS and the API still come up.
    let errors = store.reconcile().await;
    for error in &errors {
        warn!(%error, "couldn't restore");
    }
    info!(
        failures = errors.len(),
        "restored routes, docker networks and aliases"
    );

    // DNS first: it's the one likely to fail (port 53 needs root; on macOS the address may need an
    // lo0 alias), and then there's no control socket to clean up.
    Platform::ensure_local_address(*dns_address.ip())?;
    let dns_socket = dns::bind(dns_address).await?;
    let listener = Platform::bind_control(socket)?;
    let daemon = DaemonInfo {
        started_at: Timestamp::now(),
        dns_address,
    };
    info!(address = %dns_address, zone = dns::ZONE, "answering DNS (UDP)");
    // Now that it answers: point the system's resolver at it for the zone. Not fatal: `dig @…`
    // still works without it.
    match Platform::ensure_dns_routing(dns_address) {
        Ok(DnsRouting::Configured(how)) => info!("DNS routing: {how}"),
        Ok(DnsRouting::Unavailable(why)) => {
            warn!(
                "no DNS routing ({why}): only queries sent to {dns_address} directly are answered"
            );
        }
        Err(error) => warn!(%error, "couldn't set up DNS routing"),
    }
    info!(socket = %socket.display(), "serving the control API");
    info!("ready");

    // DNS reads the hosts the store keeps current.
    let hosts = store.host_table();
    // The control server stops gracefully on the signal; DNS stops when it does.
    let result = tokio::select! {
        result = control::server::serve(listener, AppState { daemon, store, users: Arc::new(SystemUsers) }, Platform::shutdown_signal()) => result,
        () = dns::serve(dns_socket, hosts) => unreachable!("DNS serves until dropped"),
    };
    info!("shutting down");
    if let Err(error) = Platform::remove_dns_routing() {
        warn!(%error, "couldn't remove DNS routing");
    }
    Platform::unbind_control(socket);
    info!("stopped");
    result
}
