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

const LAUNCHCTL: &str = "launchctl";
const PLIST_PATH: &str = "/Library/LaunchDaemons/com.krr-software-solutions.obeliskd.plist";
const LOG_PATH: &str = "/var/log/obeliskd.log";
const RESOLVER_DIR: &str = "/etc/resolver";

/// `system/<label>`: the service, to launchctl.
fn target() -> String {
    format!("system/{}", manage::LAUNCHD_LABEL)
}

fn launchctl(args: &[&str]) -> Result<(), ObeliskdError> {
    command::run_ok(LAUNCHCTL, &command::args(args), &[])
}

/// Whether launchd has it loaded (bootstrapped).
fn loaded() -> bool {
    command::run(LAUNCHCTL, &command::args(&["print", &target()])).is_ok_and(|o| o.success)
}

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
            // A /32: without a netmask, BSD ifconfig assumes the old classful mask (10.x → /8) and
            // would route the whole range into loopback.
            .args([
                "lo0",
                "alias",
                &address.to_string(),
                "netmask",
                "255.255.255.255",
                "up",
            ])
            .output()
            .map_err(|e| error(format!("couldn't run ifconfig: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(error(format!(
                "`ifconfig lo0 alias {address} netmask 255.255.255.255` failed (needs root): {}",
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

    fn release_local_address(address: Ipv4Addr) -> Result<(), ObeliskdError> {
        // "Can't assign requested address": the alias is already gone.
        crate::os::command::run_ok(
            "/sbin/ifconfig",
            &crate::os::command::args(&["lo0", "-alias", &address.to_string()]),
            &["Can't assign requested address"],
        )
    }

    fn ensure_host_subnet(_cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        // macOS has no way to make a whole range local: each ip becomes an lo0 alias instead.
        Ok(())
    }

    fn remove_host_subnet(_cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        Ok(())
    }

    fn ensure_supernet_route(_cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        Ok(())
    }

    fn remove_supernet_route(_cidr: Ipv4Net) -> Result<(), ObeliskdError> {
        Ok(())
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

    // nix has no getgrouplist here; `id -G` asks Directory Services (nested groups included).
    fn groups_of(uid: u32) -> Vec<u32> {
        let Some(user) = unix::account(uid) else {
            return Vec::new();
        };
        let primary = user.gid.as_raw();
        let args = vec!["-G".to_string(), user.name];
        match command::run("id", &args) {
            Ok(output) if output.success => output
                .stdout
                .split_whitespace()
                .filter_map(|gid| gid.parse().ok())
                .chain(std::iter::once(primary))
                .collect(),
            Ok(output) => {
                tracing::warn!(uid, error = %command::failed("id", &args, &output), "couldn't look up the user's groups");
                vec![primary]
            }
            Err(error) => {
                tracing::warn!(uid, %error, "couldn't look up the user's groups");
                vec![primary]
            }
        }
    }

    fn ensure_dns_routing(dns: SocketAddrV4) -> Result<DnsRouting, ObeliskdError> {
        let dir = Path::new(RESOLVER_DIR);
        std::fs::create_dir_all(dir).map_err(|source| ObeliskdError::StateWrite {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(ZONE);
        std::fs::write(&path, manage::resolver_file(dns)).map_err(|source| {
            ObeliskdError::StateWrite {
                path: path.clone(),
                source,
            }
        })?;
        Ok(DnsRouting::Configured(format!(
            "{} sends {ZONE} to {dns}",
            path.display()
        )))
    }

    fn remove_dns_routing() -> Result<(), ObeliskdError> {
        let path = Path::new(RESOLVER_DIR).join(ZONE);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(ObeliskdError::StateWrite { path, source: e })
            }
            _ => Ok(()),
        }
    }

    fn service_file() -> PathBuf {
        PathBuf::from(PLIST_PATH)
    }

    fn install_service(service_command: &[String]) -> Result<(), ObeliskdError> {
        let plist = manage::launchd_plist(service_command, Path::new(LOG_PATH));
        std::fs::write(PLIST_PATH, plist).map_err(|source| ObeliskdError::StateWrite {
            path: PathBuf::from(PLIST_PATH),
            source,
        })?;
        // Reload it, so a changed plist (or an upgraded binary) takes effect.
        let _ = launchctl(&["bootout", &target()]);
        launchctl(&["enable", &target()])?;
        launchctl(&["bootstrap", "system", PLIST_PATH])
    }

    fn remove_service() -> Result<(), ObeliskdError> {
        if !Path::new(PLIST_PATH).exists() {
            return Ok(());
        }
        let _ = launchctl(&["bootout", &target()]);
        std::fs::remove_file(PLIST_PATH).map_err(|source| ObeliskdError::StateWrite {
            path: PathBuf::from(PLIST_PATH),
            source,
        })
    }

    fn control_service(action: ServiceAction) -> Result<(), ObeliskdError> {
        let target = target();
        match action {
            // Not loaded (never bootstrapped since boot, e.g. disabled at the time): load it.
            ServiceAction::Start if !loaded() => launchctl(&["bootstrap", "system", PLIST_PATH]),
            ServiceAction::Start => launchctl(&["kickstart", &target]),
            // A clean exit isn't restarted (KeepAlive is only for failures).
            ServiceAction::Stop => launchctl(&["kill", "TERM", &target]),
            ServiceAction::Restart => launchctl(&["kickstart", "-k", &target]),
            ServiceAction::Enable => launchctl(&["enable", &target]),
            ServiceAction::Disable => launchctl(&["disable", &target]),
        }
    }

    fn service_state() -> ServiceState {
        let print = command::run(LAUNCHCTL, &command::args(&["print", &target()]));
        let running = print
            .as_ref()
            .is_ok_and(|o| o.success && o.stdout.contains("state = running"));
        let disabled = command::run(LAUNCHCTL, &command::args(&["print-disabled", "system"]))
            .is_ok_and(|o| {
                o.stdout.lines().any(|l| {
                    l.contains(&format!("\"{}\"", manage::LAUNCHD_LABEL))
                        && (l.contains("=> disabled") || l.contains("=> true"))
                })
            });
        let installed = Path::new(PLIST_PATH).exists();
        ServiceState {
            installed,
            enabled: installed && !disabled,
            running,
        }
    }

    fn show_logs(follow: bool) -> Result<(), ObeliskdError> {
        let mut args = command::args(&["-n", "200"]);
        if follow {
            args.push("-f".to_string());
        }
        args.push(LOG_PATH.to_string());
        command::run_attached("tail", &args)
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
