//! `obeliskd manage …`: running obeliskd as a system service. The service manager (systemd,
//! launchd) does the work through `PlatformImpl`; this drives it, and checks the result with the
//! daemon itself.

use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use clap_complete::Shell;

use obeliskd::control::api::SupernetDealloc;
use obeliskd::control::client::ControlClient;
use obeliskd::manage::{DaemonSettings, INSTALL_PATH, ServiceAction, install_binary};
use obeliskd::{ObeliskdError, Platform, PlatformImpl};

/// Where `install` puts shell completions: the system-wide directories each shell loads from on its
/// own, so no rc file needs editing.
const COMPLETIONS: [(Shell, &str); 3] = [
    (
        Shell::Bash,
        "/usr/local/share/bash-completion/completions/obeliskd",
    ),
    (Shell::Zsh, "/usr/local/share/zsh/site-functions/_obeliskd"),
    (
        Shell::Fish,
        "/usr/local/share/fish/vendor_completions.d/obeliskd.fish",
    ),
];

/// How long to wait for a (re)started daemon to answer.
const STARTUP: Duration = Duration::from_secs(15);

fn require_root(what: &str) -> Result<(), ObeliskdError> {
    if Platform::current_uid() == 0 {
        Ok(())
    } else {
        Err(ObeliskdError::NeedsRootFor(format!(
            "obeliskd manage {what}"
        )))
    }
}

fn require_installed() -> Result<(), ObeliskdError> {
    if Platform::service_state().installed {
        Ok(())
    } else {
        Err(ObeliskdError::Invalid(
            "the service isn't installed (run `sudo obeliskd manage install`)".to_string(),
        ))
    }
}

/// Copies this binary into place, installs the service with `settings`, and waits for it to
/// answer.
pub async fn install(
    settings: &DaemonSettings,
    completions: fn(Shell) -> Vec<u8>,
) -> Result<(), ObeliskdError> {
    require_root("install")?;
    let exe = std::env::current_exe()?;
    let target = Path::new(INSTALL_PATH);
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if install_binary(&exe, target)? {
        println!("installed {} (from {})", target.display(), exe.display());
    } else {
        println!("{} is already this binary", target.display());
    }
    for (shell, path) in COMPLETIONS {
        let path = Path::new(path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, completions(shell))?;
    }
    println!("installed shell completions (bash, zsh, fish) under /usr/local/share");
    Platform::install_service(&settings.command())?;
    println!(
        "installed {}; it starts now and at every boot",
        Platform::service_file().display()
    );
    wait_for_daemon(&settings.socket).await?;
    println!("the daemon answers on {}", settings.socket.display());
    println!("check DNS with `obeliskd manage status`");
    Ok(())
}

/// Removes the service and the binary; with `purge` (the state dir), every allocation first, then
/// the state.
pub async fn uninstall(socket: &Path, purge: Option<&Path>) -> Result<(), ObeliskdError> {
    require_root("uninstall")?;
    if purge.is_some() {
        let client = ControlClient::new(socket)?;
        let supernets = client.supernets().await.map_err(|e| {
            ObeliskdError::Invalid(format!(
                "--purge needs the daemon running, to remove its routes and docker networks first \
                 (try `sudo obeliskd manage start`): {e}"
            ))
        })?;
        for supernet in supernets {
            client
                .dealloc_supernet(&supernet.name, &SupernetDealloc { recursive: true })
                .await?;
            println!("removed supernet {} and everything in it", supernet.name);
        }
    }
    let had_service = Platform::service_state().installed;
    Platform::remove_service()?;
    if had_service {
        println!("removed {}", Platform::service_file().display());
    }
    // The daemon removes it on a clean stop; this covers one that didn't stop cleanly.
    Platform::remove_dns_routing()?;
    for (_, path) in COMPLETIONS {
        match std::fs::remove_file(path) {
            Ok(()) => println!("removed {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    match std::fs::remove_file(INSTALL_PATH) {
        Ok(()) => println!("removed {INSTALL_PATH}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(state_dir) = purge {
        match std::fs::remove_dir_all(state_dir) {
            Ok(()) => println!("removed {}", state_dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        println!("kept the state (allocations, grants): `uninstall --purge` removes it");
    }
    Ok(())
}

/// Start / stop / restart / enable / disable, waiting for the daemon after a (re)start.
pub async fn control(socket: &Path, action: ServiceAction) -> Result<(), ObeliskdError> {
    let what = match action {
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
        ServiceAction::Restart => "restart",
        ServiceAction::Enable => "enable",
        ServiceAction::Disable => "disable",
    };
    require_root(what)?;
    require_installed()?;
    Platform::control_service(action)?;
    match action {
        ServiceAction::Start | ServiceAction::Restart => {
            wait_for_daemon(socket).await?;
            println!("running; the daemon answers on {}", socket.display());
        }
        ServiceAction::Stop => println!("stopped"),
        ServiceAction::Enable => println!("enabled: it starts at every boot"),
        ServiceAction::Disable => println!("disabled: it won't start at boot"),
    }
    Ok(())
}

async fn wait_for_daemon(socket: &Path) -> Result<(), ObeliskdError> {
    let client = ControlClient::new(socket)?;
    let deadline = tokio::time::Instant::now() + STARTUP;
    loop {
        match client.health().await {
            Ok(_) => return Ok(()),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(ObeliskdError::Invalid(format!(
                    "the service started, but the daemon didn't answer within {}s ({error}); see \
                     `obeliskd manage logs`",
                    STARTUP.as_secs()
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

/// One line per question: the service, the binary, the daemon, and DNS through the system
/// resolver.
pub async fn status(socket: &Path) -> Result<(), ObeliskdError> {
    let state = Platform::service_state();
    let service = if state.installed {
        format!(
            "installed ({}), {}, {}",
            Platform::service_file().display(),
            if state.enabled {
                "enabled"
            } else {
                "not enabled"
            },
            if state.running {
                "running"
            } else {
                "not running"
            },
        )
    } else {
        "not installed (`sudo obeliskd manage install`)".to_string()
    };
    println!("service  {service}");

    let this = env!("CARGO_PKG_VERSION");
    let binary = match installed_version() {
        Some(version) if version == this => format!("{INSTALL_PATH} {version}"),
        Some(version) => format!(
            "{INSTALL_PATH} {version}, but this binary is {this} (re-run install to upgrade)"
        ),
        None => format!("{INSTALL_PATH}: not installed"),
    };
    println!("binary   {binary}");

    let client = ControlClient::new(socket)?;
    let Ok(health) = client.health().await else {
        println!("daemon   not answering on {}", socket.display());
        return Ok(());
    };
    println!(
        "daemon   answering on {} since {}; DNS on {}",
        socket.display(),
        health.daemon.started_at,
        health.daemon.dns_address
    );
    println!("dns      {}", dns_check(&client).await);
    Ok(())
}

/// `obeliskd 0.0.0` → `0.0.0`, from the installed binary.
fn installed_version() -> Option<String> {
    let output = std::process::Command::new(INSTALL_PATH)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

/// Resolves one of the daemon's hosts the way any program would (through the system resolver,
/// not straight to the daemon), and compares the answer. A wildcard host is checked with a made-up
/// label in place of each `*`.
async fn dns_check(client: &ControlClient) -> String {
    let Ok(tree) = client.hosts().await else {
        return "couldn't list hosts to check with".to_string();
    };
    let sample = tree
        .iter()
        .flat_map(|s| s.subnets.iter().flatten())
        .flat_map(|s| s.ips.iter().flatten())
        .find_map(|ip| {
            let host = ip.hosts.as_ref()?.first()?;
            let name = host
                .as_str()
                .split('.')
                .map(|label| if label == "*" { "status-check" } else { label })
                .collect::<Vec<_>>()
                .join(".");
            Some((name, ip.address))
        });
    let Some((name, expected)) = sample else {
        return "no hosts to check with yet (`obeliskd host alloc …`)".to_string();
    };
    match tokio::net::lookup_host((name.as_str(), 0)).await {
        Ok(addresses) => {
            let got: Vec<Ipv4Addr> = addresses
                .filter_map(|a| match a.ip() {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    std::net::IpAddr::V6(_) => None,
                })
                .collect();
            if got.contains(&expected) {
                format!("{name} → {expected} through the system resolver: ok")
            } else {
                format!(
                    "{name} resolves to {got:?} through the system resolver, not {expected}: DNS \
                     routing isn't reaching the daemon (see `obeliskd manage logs`)"
                )
            }
        }
        Err(error) => format!(
            "{name} doesn't resolve through the system resolver ({error}): DNS routing isn't in \
             place (see `obeliskd manage logs`)"
        ),
    }
}
