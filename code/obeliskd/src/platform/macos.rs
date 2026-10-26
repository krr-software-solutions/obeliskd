use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};

use ipnet::Ipv4Net;

use tokio::net::{UnixListener, UnixStream};

use super::{PlatformImpl, unix};
use crate::ObeliskdError;
use crate::control::Peer;
use crate::os::command;

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
