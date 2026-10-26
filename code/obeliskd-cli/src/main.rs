use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use ipnet::Ipv4Net;
use obeliskd::control::api::{
    HostAlloc, IpAlloc, IpDealloc, IpDeallocTagged, IpQuery, SubnetAlloc, SubnetDealloc,
    SubnetQuery, SupernetAlloc, SupernetDealloc,
};
use obeliskd::control::client::ControlClient;
use obeliskd::model::{Hostname, Name, SubnetKind};
use obeliskd::{ObeliskdError, Platform, PlatformImpl};
use serde::Serialize;

mod pretty;

//

/// A small, hand-written DNS server.
#[derive(Debug, Parser)]
#[command(name = "obeliskd", version)]
struct Cli {
    /// The daemon's control socket
    #[arg(
        long,
        global = true,
        help_heading = SPECIAL,
        env = "OBELISKD_SOCKET",
        default_value_os_t = Platform::control_socket_path()
    )]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run and manage the daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Supernets: the logical ranges subnets are carved from
    Supernet {
        #[command(subcommand)]
        command: SupernetCommand,
    },
    /// Subnets: ranges carved from a supernet, each `host` or `docker`
    Subnet {
        #[command(subcommand)]
        command: SubnetCommand,
    },
    /// Ips: named addresses in a subnet, picked by obeliskd
    Ip {
        #[command(subcommand)]
        command: IpCommand,
    },
    /// Hosts: hostnames under obd.internal, each resolving to an ip
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground, serving the control socket
    Run {
        /// Where to answer DNS
        #[arg(
            long,
            value_name = "IPV4:PORT",
            env = "OBELISKD_DNS_ADDRESS",
            default_value_t = Platform::default_dns_address()
        )]
        dns_address: SocketAddrV4,
        /// Where the daemon keeps its state (config.toml, ...)
        #[arg(
            long,
            env = "OBELISKD_STATE_DIR",
            default_value_os_t = Platform::state_dir()
        )]
        state_dir: PathBuf,
        /// What to log: a level (`info`, `debug`, …) or a filter like `info,obeliskd::dns=warn`
        #[arg(long, env = "OBELISKD_LOG", default_value = "info")]
        log_level: String,
    },
    /// Ask the running daemon how it's doing, and who it sees you as
    Health {
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
enum SupernetCommand {
    /// List every supernet
    List {
        #[command(flatten)]
        output: Output,
    },
    /// Create a supernet (root only)
    Alloc {
        /// Its name, e.g. `obd`
        #[arg(long)]
        supernet: Name,
        /// Its range, e.g. `10.57.0.0/16`
        #[arg(long)]
        cidr: Ipv4Net,
        #[command(flatten)]
        output: Output,
    },
    /// Remove a supernet (root only)
    Dealloc {
        /// The supernet to remove
        #[arg(long)]
        supernet: Name,
        /// Also remove everything in it: subnets, ips and hosts (otherwise it must have no subnets)
        #[arg(long)]
        recursive: bool,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
enum SubnetCommand {
    /// List subnets (all, or one supernet's)
    List {
        /// Only this supernet's subnets
        #[arg(long)]
        supernet: Option<Name>,
        #[command(flatten)]
        output: Output,
    },
    /// Carve the first free range of a size out of a supernet (root only)
    Alloc {
        /// The supernet to carve it from
        #[arg(long)]
        supernet: Name,
        /// Its name, unique within the supernet, e.g. `k3d-dev`
        #[arg(long)]
        subnet: Name,
        /// What its addresses are for
        #[arg(long, value_enum)]
        kind: KindArg,
        /// Its size as a prefix length, e.g. `24`; obeliskd picks the first free range
        #[arg(long, value_name = "PREFIX-LENGTH")]
        size: u8,
        /// Docker only, and required there: the size of Docker's own slice at the start of the
        /// subnet, as a prefix length (e.g. `26` in a /24: Docker gets .0–.63)
        #[arg(long, value_name = "PREFIX-LENGTH", required_if_eq("kind", "docker"))]
        reserve_size: Option<u8>,
        #[command(flatten)]
        output: Output,
    },
    /// Remove a subnet from its supernet (root only)
    Dealloc {
        /// The supernet it's in
        #[arg(long)]
        supernet: Name,
        /// The subnet to remove
        #[arg(long)]
        subnet: Name,
        /// Also remove its ips and their hosts (otherwise it must have no ips)
        #[arg(long)]
        recursive: bool,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// List every host, with the ip and address it resolves to
    List {
        #[command(flatten)]
        output: Output,
    },
    /// Point a hostname at an ip (root only)
    Alloc {
        /// The full hostname, e.g. `app.obd.internal`
        #[arg(long)]
        host: Hostname,
        /// The supernet the ip is in
        #[arg(long)]
        supernet: Name,
        /// The subnet the ip is in
        #[arg(long)]
        subnet: Name,
        /// The ip it resolves to
        #[arg(long)]
        ip: Name,
        #[command(flatten)]
        output: Output,
    },
    /// Remove a hostname (root only)
    Dealloc {
        /// The full hostname, e.g. `app.obd.internal`
        #[arg(long)]
        host: Hostname,
        #[command(flatten)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
enum IpCommand {
    /// List ips (all, one supernet's, or one subnet's)
    List {
        /// Only this supernet's ips
        #[arg(long)]
        supernet: Option<Name>,
        /// Only this subnet's ips (needs --supernet)
        #[arg(long, requires = "supernet")]
        subnet: Option<Name>,
        /// Only ips with this tag
        #[arg(long)]
        tag: Option<Name>,
        #[command(flatten)]
        output: Output,
    },
    /// Give a name the lowest free address in a subnet (root only)
    Alloc {
        /// The supernet the subnet is in
        #[arg(long)]
        supernet: Name,
        /// The subnet to take the address from
        #[arg(long)]
        subnet: Name,
        /// Its name, unique within the subnet, e.g. `db`
        #[arg(long)]
        ip: Name,
        /// A group label, e.g. `pr-42`, to find (and later remove) a set of ips together
        #[arg(long)]
        tag: Option<Name>,
        #[command(flatten)]
        output: Output,
    },
    /// Free an ip, or every ip with a tag; their addresses can be handed out again (root only)
    #[command(group = clap::ArgGroup::new("which").args(["ip", "tag"]).required(true))]
    Dealloc {
        /// The supernet the subnet is in (with --tag: only ips in this supernet)
        #[arg(long)]
        supernet: Option<Name>,
        /// The subnet it's in (with --tag: only ips in this subnet; needs --supernet)
        #[arg(long, requires = "supernet")]
        subnet: Option<Name>,
        /// The ip to free (needs --supernet and --subnet)
        #[arg(long, requires_all = ["supernet", "subnet"])]
        ip: Option<Name>,
        /// Free every ip with this tag, e.g. `pr-42`, all at once
        #[arg(long)]
        tag: Option<Name>,
        /// Also remove the hosts pointing at them (otherwise none may have any)
        #[arg(long)]
        recursive: bool,
        #[command(flatten)]
        output: Output,
    },
}

/// `--kind`: what a subnet's addresses are for.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum KindArg {
    /// Owned by this machine: host apps bind them
    Host,
    /// A Docker network: containers and MetalLB-pinned Services claim them
    Docker,
}

impl From<KindArg> for SubnetKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Host => SubnetKind::Host,
            KindArg::Docker => SubnetKind::Docker,
        }
    }
}

/// How a client command prints the daemon's answer.
#[derive(Debug, Args)]
#[command(next_help_heading = EXTRA)]
struct Output {
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Pretty)]
    format: Format,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    Pretty,
    Json,
}

impl Output {
    fn print<T: Serialize>(&self, value: &T, pretty: impl FnOnce(&T) -> String) {
        match self.format {
            Format::Pretty => print!("{}", pretty(value)),
            Format::Json => {
                let json = serde_json::to_string_pretty(value).expect("API types serialize");
                println!("{json}");
            }
        }
    }
}

//

async fn command_daemon_run(
    socket: &Path,
    dns_address: SocketAddrV4,
    state_dir: &Path,
) -> Result<(), ObeliskdError> {
    obeliskd::daemon::run(socket, dns_address, state_dir).await
}

async fn command_daemon_health(socket: &Path, output: &Output) -> Result<(), ObeliskdError> {
    let health = ControlClient::new(socket)?.health().await?;
    output.print(&health, pretty::health);
    Ok(())
}

async fn command_supernet_list(socket: &Path, output: &Output) -> Result<(), ObeliskdError> {
    let supernets = ControlClient::new(socket)?.supernets().await?;
    output.print(&supernets, |s| pretty::supernets(s));
    Ok(())
}

async fn command_supernet_alloc(
    socket: &Path,
    name: Name,
    cidr: Ipv4Net,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let supernet = ControlClient::new(socket)?
        .alloc_supernet(&SupernetAlloc { name, cidr })
        .await?;
    output.print(&supernet, pretty::supernet_allocated);
    Ok(())
}

async fn command_subnet_list(
    socket: &Path,
    supernet: Option<Name>,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let tree = ControlClient::new(socket)?
        .subnets(&SubnetQuery { supernet })
        .await?;
    output.print(&tree, |t| pretty::subnets(t));
    Ok(())
}

async fn command_subnet_alloc(
    socket: &Path,
    request: SubnetAlloc,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let subnet = ControlClient::new(socket)?.alloc_subnet(&request).await?;
    output.print(&subnet, pretty::subnet_allocated);
    Ok(())
}

async fn command_supernet_dealloc(
    socket: &Path,
    supernet: &Name,
    recursive: bool,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let removed = ControlClient::new(socket)?
        .dealloc_supernet(supernet, &SupernetDealloc { recursive })
        .await?;
    output.print(&removed, pretty::supernet_deallocated);
    Ok(())
}

async fn command_subnet_dealloc(
    socket: &Path,
    supernet: &Name,
    subnet: &Name,
    recursive: bool,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let removed = ControlClient::new(socket)?
        .dealloc_subnet(supernet, subnet, &SubnetDealloc { recursive })
        .await?;
    output.print(&removed, pretty::subnet_deallocated);
    Ok(())
}

async fn command_ip_list(
    socket: &Path,
    query: IpQuery,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let tree = ControlClient::new(socket)?.ips(&query).await?;
    output.print(&tree, |t| pretty::ips(t));
    Ok(())
}

async fn command_ip_alloc(
    socket: &Path,
    request: IpAlloc,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let ip = ControlClient::new(socket)?.alloc_ip(&request).await?;
    output.print(&ip, pretty::ip_allocated);
    Ok(())
}

async fn command_ip_dealloc(
    socket: &Path,
    supernet: &Name,
    subnet: &Name,
    ip: &Name,
    recursive: bool,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let removed = ControlClient::new(socket)?
        .dealloc_ip(supernet, subnet, ip, &IpDealloc { recursive })
        .await?;
    output.print(&removed, pretty::ip_deallocated);
    Ok(())
}

async fn command_ip_dealloc_tagged(
    socket: &Path,
    request: &IpDeallocTagged,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let removed = ControlClient::new(socket)?
        .dealloc_ips_tagged(request)
        .await?;
    output.print(&removed, |r| pretty::ips_deallocated(&request.tag, r));
    Ok(())
}

async fn command_host_list(socket: &Path, output: &Output) -> Result<(), ObeliskdError> {
    let hosts = ControlClient::new(socket)?.hosts().await?;
    output.print(&hosts, |h| pretty::hosts(h));
    Ok(())
}

async fn command_host_alloc(
    socket: &Path,
    request: HostAlloc,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let host = ControlClient::new(socket)?.alloc_host(&request).await?;
    output.print(&host, pretty::host_allocated);
    Ok(())
}

async fn command_host_dealloc(
    socket: &Path,
    host: &Hostname,
    output: &Output,
) -> Result<(), ObeliskdError> {
    let removed = ControlClient::new(socket)?.dealloc_host(host).await?;
    output.print(&removed, pretty::host_deallocated);
    Ok(())
}

/// The daemon logs to stderr (journald, under systemd); client commands don't log.
fn init_logging(filter: &str) -> Result<(), ObeliskdError> {
    let filter = tracing_subscriber::EnvFilter::try_new(filter).map_err(|e| {
        ObeliskdError::Invalid(format!("--log-level {filter:?} isn't a valid filter: {e}"))
    })?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        // Colors only on a terminal, not in journald or a file.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .init();
    Ok(())
}

fn command_completions(shell: clap_complete::Shell) {
    let mut cmd = command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

//

// --help groups options under three headings. Only the exceptions are labelled where they're
// declared (`--socket`, `Output`); `command()` files everything else.

/// What this command is about.
const CORE: &str = "Core Options";
/// Presentation and tweaks shared by many commands.
const EXTRA: &str = "Extra Options";
/// Plumbing present on every command.
const SPECIAL: &str = "Special Options";

/// The CLI definition with headings applied: anything unlabelled is Core, and the built-in
/// `--help` / `--version` are Special.
fn command() -> clap::Command {
    fn apply(cmd: clap::Command) -> clap::Command {
        cmd.mut_args(|arg| {
            if arg.get_help_heading().is_some() {
                arg
            } else if matches!(arg.get_id().as_str(), "help" | "version") {
                arg.help_heading(SPECIAL)
            } else {
                arg.help_heading(CORE)
            }
        })
        .mut_subcommands(apply)
    }
    // Built first so the auto-generated --help/--version exist for `apply` to move.
    let mut cmd = Cli::command();
    cmd.build();
    apply(cmd)
}

async fn run_daemon(socket: &Path, command: DaemonCommand) -> Result<(), ObeliskdError> {
    match command {
        DaemonCommand::Run {
            dns_address,
            state_dir,
            log_level,
        } => {
            init_logging(&log_level)?;
            command_daemon_run(socket, dns_address, &state_dir).await
        }
        DaemonCommand::Health { output } => command_daemon_health(socket, &output).await,
    }
}

async fn run_supernet(socket: &Path, command: SupernetCommand) -> Result<(), ObeliskdError> {
    match command {
        SupernetCommand::List { output } => command_supernet_list(socket, &output).await,
        SupernetCommand::Alloc {
            supernet,
            cidr,
            output,
        } => command_supernet_alloc(socket, supernet, cidr, &output).await,
        SupernetCommand::Dealloc {
            supernet,
            recursive,
            output,
        } => command_supernet_dealloc(socket, &supernet, recursive, &output).await,
    }
}

async fn run_subnet(socket: &Path, command: SubnetCommand) -> Result<(), ObeliskdError> {
    match command {
        SubnetCommand::List { supernet, output } => {
            command_subnet_list(socket, supernet, &output).await
        }
        SubnetCommand::Alloc {
            supernet,
            subnet,
            kind,
            size,
            reserve_size,
            output,
        } => {
            let request = SubnetAlloc {
                supernet,
                name: subnet,
                kind: kind.into(),
                size,
                reserve_size,
            };
            command_subnet_alloc(socket, request, &output).await
        }
        SubnetCommand::Dealloc {
            supernet,
            subnet,
            recursive,
            output,
        } => command_subnet_dealloc(socket, &supernet, &subnet, recursive, &output).await,
    }
}

async fn run_ip(socket: &Path, command: IpCommand) -> Result<(), ObeliskdError> {
    match command {
        IpCommand::List {
            supernet,
            subnet,
            tag,
            output,
        } => {
            let query = IpQuery {
                supernet,
                subnet,
                tag,
            };
            command_ip_list(socket, query, &output).await
        }
        IpCommand::Alloc {
            supernet,
            subnet,
            ip,
            tag,
            output,
        } => {
            let request = IpAlloc {
                supernet,
                subnet,
                name: ip,
                tag,
            };
            command_ip_alloc(socket, request, &output).await
        }
        IpCommand::Dealloc {
            supernet,
            subnet,
            ip,
            tag,
            recursive,
            output,
        } => match (supernet, subnet, ip, tag) {
            (Some(supernet), Some(subnet), Some(ip), _) => {
                command_ip_dealloc(socket, &supernet, &subnet, &ip, recursive, &output).await
            }
            (supernet, subnet, _, Some(tag)) => {
                let request = IpDeallocTagged {
                    tag,
                    supernet,
                    subnet,
                    recursive,
                };
                command_ip_dealloc_tagged(socket, &request, &output).await
            }
            _ => unreachable!("clap requires --ip (with --supernet and --subnet) or --tag"),
        },
    }
}

async fn run_host(socket: &Path, command: HostCommand) -> Result<(), ObeliskdError> {
    match command {
        HostCommand::List { output } => command_host_list(socket, &output).await,
        HostCommand::Alloc {
            host,
            supernet,
            subnet,
            ip,
            output,
        } => {
            let request = HostAlloc {
                name: host,
                supernet,
                subnet,
                ip,
            };
            command_host_alloc(socket, request, &output).await
        }
        HostCommand::Dealloc { host, output } => command_host_dealloc(socket, &host, &output).await,
    }
}

async fn main_impl() -> Result<(), ObeliskdError> {
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    let socket: PathBuf = cli.socket;

    match cli.command {
        Command::Daemon { command } => run_daemon(&socket, command).await,
        Command::Supernet { command } => run_supernet(&socket, command).await,
        Command::Subnet { command } => run_subnet(&socket, command).await,
        Command::Ip { command } => run_ip(&socket, command).await,
        Command::Host { command } => run_host(&socket, command).await,
        Command::Completions { shell } => {
            command_completions(shell);
            Ok(())
        }
    }
}

// Not one worker per core (tokio's default): DNS is a single receive loop and the control API sees a
// handful of requests, so a few workers is plenty, and each one costs a stack and an allocator arena.
#[tokio::main(worker_threads = 4)]
async fn main() -> ExitCode {
    match main_impl().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR - {error}");
            ExitCode::FAILURE
        }
    }
}
