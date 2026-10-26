use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;

use tokio::net::{UnixListener, UnixStream};

use super::{PlatformImpl, unix};
use crate::ObeliskdError;
use crate::control::Peer;
use crate::dns::ZONE;
use crate::manage::{self, DnsRouting, ServiceAction, ServiceState};
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

    fn user_id(name: &str) -> Option<u32> {
        unix::user_id(name)
    }

    fn user_name(uid: u32) -> Option<String> {
        unix::user_name(uid)
    }

    fn group_id(name: &str) -> Option<u32> {
        unix::group_id(name)
    }

    fn group_name(gid: u32) -> Option<String> {
        unix::group_name(gid)
    }

    fn groups_of(uid: u32) -> Vec<u32> {
        let Some(user) = unix::account(uid) else {
            return Vec::new();
        };
        let Ok(name) = std::ffi::CString::new(user.name) else {
            return Vec::new();
        };
        match nix::unistd::getgrouplist(&name, user.gid) {
            Ok(gids) => gids.into_iter().map(nix::unistd::Gid::as_raw).collect(),
            Err(error) => {
                tracing::warn!(uid, %error, "couldn't look up the user's groups");
                vec![user.gid.as_raw()]
            }
        }
    }

    fn ensure_dns_routing(dns: SocketAddrV4) -> Result<DnsRouting, ObeliskdError> {
        let resolved = command::run(
            SYSTEMCTL,
            &command::args(&["is-active", "--quiet", "systemd-resolved"]),
        );
        if !resolved.is_ok_and(|o| o.success) {
            return Ok(DnsRouting::Unavailable(
                "systemd-resolved isn't running".to_string(),
            ));
        }
        for args in dns_link_args(dns) {
            command::run_ok(IP, &args, &["File exists"])?;
        }
        let server = dns_server(dns);
        for args in resolvectl_args(&server) {
            command::run_ok(RESOLVECTL, &args, &[])?;
        }
        Ok(DnsRouting::Configured(format!(
            "systemd-resolved sends ~{ZONE} to {server} (link {DNS_LINK})"
        )))
    }

    fn remove_dns_routing() -> Result<(), ObeliskdError> {
        // resolved forgets the link's settings with the link.
        command::run_ok(
            IP,
            &command::args(&["link", "del", DNS_LINK]),
            &["Cannot find device"],
        )
    }

    fn service_file() -> PathBuf {
        PathBuf::from(UNIT_PATH)
    }

    fn install_service(service_command: &[String]) -> Result<(), ObeliskdError> {
        let was_running = Self::service_state().running;
        write_file(
            Path::new(UNIT_PATH),
            &manage::systemd_unit(service_command),
            0o644,
        )?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", manage::SERVICE])?;
        if was_running {
            systemctl(&["restart", manage::SERVICE])?;
        }
        Ok(())
    }

    fn remove_service() -> Result<(), ObeliskdError> {
        if !Path::new(UNIT_PATH).exists() {
            return Ok(());
        }
        systemctl(&["disable", "--now", manage::SERVICE])?;
        std::fs::remove_file(UNIT_PATH).map_err(|source| ObeliskdError::StateWrite {
            path: PathBuf::from(UNIT_PATH),
            source,
        })?;
        systemctl(&["daemon-reload"])
    }

    fn control_service(action: ServiceAction) -> Result<(), ObeliskdError> {
        let verb = match action {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
            ServiceAction::Enable => "enable",
            ServiceAction::Disable => "disable",
        };
        systemctl(&[verb, manage::SERVICE])
    }

    fn service_state() -> ServiceState {
        let says = |verb: &str, expected: &str| {
            command::run(SYSTEMCTL, &command::args(&[verb, manage::SERVICE]))
                .is_ok_and(|o| o.stdout.trim() == expected)
        };
        ServiceState {
            installed: Path::new(UNIT_PATH).exists(),
            enabled: says("is-enabled", "enabled"),
            running: says("is-active", "active"),
        }
    }

    fn show_logs(follow: bool) -> Result<(), ObeliskdError> {
        let mut args = command::args(&["--unit", manage::SERVICE, "--no-pager", "--lines", "200"]);
        if follow {
            args.push("--follow".to_string());
        }
        command::run_attached("journalctl", &args)
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
const SYSTEMCTL: &str = "systemctl";
const RESOLVECTL: &str = "resolvectl";

const UNIT_PATH: &str = "/etc/systemd/system/obeliskd.service";

/// The dummy link resolved routes the zone through.
const DNS_LINK: &str = "obd0";

fn systemctl(args: &[&str]) -> Result<(), ObeliskdError> {
    command::run_ok(SYSTEMCTL, &command::args(args), &[])
}

/// The dummy link, carrying the DNS address. resolved only uses a link's DNS servers if the link
/// is up and has a routable address, so the address gets `scope global` (`ip` would give a
/// 127.x address `scope host`); with it on the link, queries sent out the link reach the daemon.
fn dns_link_args(dns: SocketAddrV4) -> Vec<Vec<String>> {
    let address = format!("{}/32", dns.ip());
    vec![
        command::args(&["link", "add", DNS_LINK, "type", "dummy"]),
        command::args(&[
            "addr", "replace", &address, "dev", DNS_LINK, "scope", "global",
        ]),
        command::args(&["link", "set", DNS_LINK, "up"]),
    ]
}

/// `ip` or `ip:port` (resolvectl takes a port after the address).
fn dns_server(dns: SocketAddrV4) -> String {
    if dns.port() == 53 {
        dns.ip().to_string()
    } else {
        dns.to_string()
    }
}

/// The zone, and only the zone, goes to the daemon: a routing domain (`~`), not a default route.
/// The server comes last: from the moment a link has one, resolved may send it queries, so the
/// routing domain and `default-route false` must already be in place (otherwise unrelated
/// lookups made during startup reach the daemon and get REFUSED).
fn resolvectl_args(server: &str) -> Vec<Vec<String>> {
    let domain = format!("~{ZONE}");
    vec![
        command::args(&["default-route", DNS_LINK, "false"]),
        command::args(&["domain", DNS_LINK, &domain]),
        command::args(&["llmnr", DNS_LINK, "false"]),
        command::args(&["mdns", DNS_LINK, "false"]),
        command::args(&["dns", DNS_LINK, server]),
    ]
}

/// Writes `body` to `path` with `mode`, through a temp file and a rename.
fn write_file(path: &Path, body: &str, mode: u32) -> Result<(), ObeliskdError> {
    use std::os::unix::fs::PermissionsExt;
    let error = |source| ObeliskdError::StateWrite {
        path: path.to_path_buf(),
        source,
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(error)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(error)?;
    std::fs::rename(&tmp, path).map_err(error)
}

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
    fn dns_routing_commands_send_only_the_zone_to_the_daemon() {
        let dns: SocketAddrV4 = "127.0.0.79:53".parse().unwrap();
        let link: Vec<String> = dns_link_args(dns).iter().map(|a| a.join(" ")).collect();
        assert_eq!(
            link,
            [
                "link add obd0 type dummy",
                "addr replace 127.0.0.79/32 dev obd0 scope global",
                "link set obd0 up",
            ]
        );
        assert_eq!(dns_server(dns), "127.0.0.79");
        assert_eq!(
            dns_server("127.0.0.79:5379".parse().unwrap()),
            "127.0.0.79:5379"
        );
        let resolved: Vec<String> = resolvectl_args("127.0.0.79")
            .iter()
            .map(|a| a.join(" "))
            .collect();
        assert_eq!(
            resolved,
            [
                "default-route obd0 false",
                "domain obd0 ~obd.internal",
                "llmnr obd0 false",
                "mdns obd0 false",
                "dns obd0 127.0.0.79",
            ]
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
