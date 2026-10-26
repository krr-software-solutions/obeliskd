//! Running obeliskd as a system service (`obeliskd manage …`): what's installed where, and the
//! files that describe it. The OS-specific steps (systemd, launchd) are [`PlatformImpl`]
//! methods; this module holds what they share, as plain functions that are easy to test.
//!
//! [`PlatformImpl`]: crate::PlatformImpl

use std::fmt::Write;
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};

use crate::ObeliskdError;

/// Where `manage install` puts the binary the service runs.
pub const INSTALL_PATH: &str = "/usr/local/bin/obeliskd";

/// The service's name: the systemd unit (`obeliskd.service`) and the end of the launchd label.
pub const SERVICE: &str = "obeliskd";

/// The launchd label (macOS).
pub const LAUNCHD_LABEL: &str = "com.krr-software-solutions.obeliskd";

/// What the service manager is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    /// Start at boot (doesn't start it now).
    Enable,
    /// Don't start at boot (doesn't stop it now).
    Disable,
}

/// The service, as the service manager sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceState {
    /// Its unit / plist is in place.
    pub installed: bool,
    /// It starts at boot.
    pub enabled: bool,
    pub running: bool,
}

/// Whether the daemon could point the system's resolver at itself for the zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsRouting {
    /// Set up; says how.
    Configured(String),
    /// Not possible here (e.g. no systemd-resolved); says why.
    Unavailable(String),
}

/// The daemon's settings, as `daemon run` takes them: what the service is started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonSettings {
    pub socket: PathBuf,
    pub dns_address: SocketAddrV4,
    pub state_dir: PathBuf,
    pub log_level: String,
}

impl DaemonSettings {
    /// The service's command line: the installed binary, `daemon run`, and every setting spelled
    /// out (defaults included, so the unit says exactly what runs).
    #[must_use]
    pub fn command(&self) -> Vec<String> {
        vec![
            INSTALL_PATH.to_string(),
            "--socket".to_string(),
            self.socket.display().to_string(),
            "daemon".to_string(),
            "run".to_string(),
            "--dns-address".to_string(),
            self.dns_address.to_string(),
            "--state-dir".to_string(),
            self.state_dir.display().to_string(),
            "--log-level".to_string(),
            self.log_level.clone(),
        ]
    }
}

/// `/etc/systemd/system/obeliskd.service`. Ordered after resolved and docker (when they exist;
/// neither is required), restarted if it fails, started at boot once enabled.
#[must_use]
pub fn systemd_unit(command: &[String]) -> String {
    let exec = command
        .iter()
        .map(|arg| systemd_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "\
# Managed by obeliskd (`obeliskd manage install`). Don't edit by hand: re-run install instead.
[Unit]
Description=obeliskd: local-dev DNS and IP allocation
After=network.target systemd-resolved.service docker.service

[Service]
ExecStart={exec}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
"
    )
}

/// systemd splits `ExecStart=` like a shell: quote anything with spaces or quotes in it.
fn systemd_quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
    {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// The launchd plist (macOS). `KeepAlive` restarts it only after a failure, so a clean stop
/// (SIGTERM, exit 0) stays stopped, as on Linux. Its stderr (the log) goes to `log`.
#[must_use]
pub fn launchd_plist(command: &[String], log: &Path) -> String {
    let args = command.iter().fold(String::new(), |mut out, arg| {
        let _ = writeln!(out, "        <string>{}</string>", xml_escape(arg));
        out
    });
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Managed by obeliskd (`obeliskd manage install`). Don't edit by hand: re-run install instead. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/etc/resolver/obd.internal` (macOS): send the zone to the daemon.
#[must_use]
pub fn resolver_file(dns: SocketAddrV4) -> String {
    let mut out = format!(
        "# Managed by obeliskd: written at daemon start, removed at stop.\nnameserver {}\n",
        dns.ip()
    );
    if dns.port() != 53 {
        let _ = writeln!(out, "port {}", dns.port());
    }
    out
}

/// Copies `from` to `to` (mode `0755`) through a temp file and a rename, so a running copy of the
/// old binary is never written into. Does nothing if they're the same file.
///
/// # Errors
///
/// [`ObeliskdError::StateWrite`] if it can't be copied.
pub fn install_binary(from: &Path, to: &Path) -> Result<bool, ObeliskdError> {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same(from, to) {
        return Ok(false);
    }
    let error = |source| ObeliskdError::StateWrite {
        path: to.to_path_buf(),
        source,
    };
    let tmp = to.with_extension("tmp");
    std::fs::copy(from, &tmp).map_err(error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(error)?;
    }
    std::fs::rename(&tmp, to).map_err(error)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> DaemonSettings {
        DaemonSettings {
            socket: PathBuf::from("/run/obeliskd/obeliskd.sock"),
            dns_address: "127.0.0.79:53".parse().unwrap(),
            state_dir: PathBuf::from("/Library/Application Support/obeliskd"),
            log_level: "info".to_string(),
        }
    }

    #[test]
    fn the_unit_runs_the_installed_binary_with_every_setting() {
        let unit = systemd_unit(&settings().command());
        assert!(
            unit.contains(
                "ExecStart=/usr/local/bin/obeliskd --socket /run/obeliskd/obeliskd.sock daemon run \
                 --dns-address 127.0.0.79:53 --state-dir \"/Library/Application Support/obeliskd\" \
                 --log-level info\n"
            ),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_quoting_only_touches_what_needs_it() {
        assert_eq!(systemd_quote("plain"), "plain");
        assert_eq!(systemd_quote("a b"), "\"a b\"");
        assert_eq!(systemd_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(systemd_quote(""), "\"\"");
    }

    #[test]
    fn the_plist_keeps_it_alive_only_after_failures() {
        let plist = launchd_plist(&settings().command(), Path::new("/var/log/obeliskd.log"));
        assert!(plist.contains("<string>com.krr-software-solutions.obeliskd</string>"));
        assert!(plist.contains("        <string>/Library/Application Support/obeliskd</string>\n"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert_eq!(xml_escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn the_resolver_file_names_a_port_only_when_it_isnt_53() {
        assert!(!resolver_file("127.0.0.79:53".parse().unwrap()).contains("port"));
        assert!(
            resolver_file("127.0.0.79:5379".parse().unwrap())
                .ends_with("nameserver 127.0.0.79\nport 5379\n")
        );
    }

    #[test]
    fn installing_copies_and_skips_itself() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("built"), dir.path().join("installed"));
        std::fs::write(&from, b"#!/bin/true\n").unwrap();
        assert!(install_binary(&from, &to).unwrap());
        assert_eq!(std::fs::read(&to).unwrap(), b"#!/bin/true\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&to).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        assert!(!install_binary(&to, &to).unwrap(), "the same file");
    }
}
