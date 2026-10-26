use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;

use tokio::net::{UnixListener, UnixStream};

use super::{PlatformImpl, unix};
use crate::ObeliskdError;
use crate::control::Peer;
use crate::os::command;

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
        // All of 127.0.0.0/8 is local on Linux, and so is every host subnet (its `local` route);
        // obeliskd doesn't add single addresses.
        if unix::is_local(address) {
            Ok(())
        } else {
            Err(ObeliskdError::LocalAddress {
                address,
                message: "it's not in 127.0.0.0/8 or a host subnet whose `local` route is in place"
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

    fn release_local_address(_address: Ipv4Addr) -> Result<(), ObeliskdError> {
        // Host-subnet addresses are local through the subnet's `local` route, not per address.
        Ok(())
    }

    fn ensure_host_subnet(cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        command::run_ok(IP, &host_subnet_args("replace", cidr), &[])
    }

    fn remove_host_subnet(cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        command::run_ok(IP, &host_subnet_args("del", cidr), &[NO_SUCH_ROUTE])
    }

    fn ensure_supernet_route(cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        command::run_ok(IP, &supernet_route_args("replace", cidr), &[])
    }

    fn remove_supernet_route(cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        command::run_ok(IP, &supernet_route_args("del", cidr), &[NO_SUCH_ROUTE])
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

const IP: &str = "ip";

/// What `ip route del` says when the route is already gone.
const NO_SUCH_ROUTE: &str = "No such process";

/// The metric of the supernet catch-all: high, so any more specific route always wins anyway.
const SUPERNET_ROUTE_METRIC: &str = "9999";

/// `ip route <verb> local <cidr> dev lo table local`: the whole range is this machine's own.
/// `replace` adds it or leaves it as is, so it's idempotent.
fn host_subnet_args(verb: &str, cidr: Ipv4Net) -> Vec<String> {
    command::args(&[
        "route",
        verb,
        "local",
        &cidr.to_string(),
        "dev",
        "lo",
        "table",
        "local",
    ])
}

/// `ip route <verb> unreachable <cidr> metric 9999`: unallocated addresses fail instantly.
fn supernet_route_args(verb: &str, cidr: Ipv4Net) -> Vec<String> {
    command::args(&[
        "route",
        verb,
        "unreachable",
        &cidr.to_string(),
        "metric",
        SUPERNET_ROUTE_METRIC,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_commands() {
        let cidr: Ipv4Net = "10.57.0.0/24".parse().unwrap();
        assert_eq!(
            host_subnet_args("replace", cidr).join(" "),
            "route replace local 10.57.0.0/24 dev lo table local"
        );
        let supernet: Ipv4Net = "10.57.0.0/16".parse().unwrap();
        assert_eq!(
            supernet_route_args("del", supernet).join(" "),
            "route del unreachable 10.57.0.0/16 metric 9999"
        );
    }

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
