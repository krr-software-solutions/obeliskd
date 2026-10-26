use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use ipnet::Ipv4Net;
use obeliskd::control::api::{
    GrantAdd, GrantCheckQuery, GrantQuery, HostAlloc, IpAlloc, IpDealloc, IpDeallocTagged, IpQuery,
    SubnetAlloc, SubnetDealloc, SubnetQuery, SupernetAlloc, SupernetDealloc, WhoArg,
};
use obeliskd::control::client::ControlClient;
use obeliskd::model::{Hostname, Name, Role, SubnetKind};
use obeliskd::{ObeliskdError, Platform, PlatformImpl};
use serde::Serialize;

mod pretty;
mod status;

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
    /// Grants: who may change what (a role on the whole tree, a supernet, or a subnet)
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
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
    /// An overview of each supernet: sizes, subnets, and a grid of which blocks are taken
    Status {
        /// Only this supernet
        #[arg(long)]
        supernet: Option<Name>,
        /// The grid's block size, as a prefix length (default: the supernet's + 8, i.e. 256 blocks)
        #[arg(long, value_name = "PREFIX-LENGTH")]
        block_size: Option<u8>,
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
    /// Carve the first free range of a size out of a supernet (needs manage on the supernet)
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
    /// Remove a subnet from its supernet (needs manage on the supernet)
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
    /// Print the address a name resolves to, as DNS answers it (wildcards included), for scripts:
    /// exits 0 if something matches, 1 if not, 2 on any other error
    Resolve {
        /// The full name to resolve, e.g. `app.obd.internal`
        #[arg(long)]
        host: Hostname,
        #[command(flatten)]
        output: ResolveOutput,
    },
    /// Point a hostname at an ip (needs use on its subnet)
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
    /// Remove a hostname (needs use on its subnet)
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
    /// Print one ip's address, for scripts: exits 0 if it exists, 1 if not (supernet, subnet or
    /// ip), 2 on any other error
    Resolve {
        /// The supernet the subnet is in
        #[arg(long)]
        supernet: Name,
        /// The subnet it's in
        #[arg(long)]
        subnet: Name,
        /// The ip
        #[arg(long)]
        ip: Name,
        #[command(flatten)]
        output: ResolveOutput,
    },
    /// Give a name the lowest free address in a subnet (needs use on the subnet)
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
    /// Free an ip, or every ip with a tag; their addresses can be handed out again (needs use on
    /// each subnet)
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

#[derive(Debug, Subcommand)]
enum GrantCommand {
    /// List the grants on a scope and everything inside it (all of them, by default)
    List {
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        who: WhoFilter,
        #[command(flatten)]
        output: Output,
    },
    /// Give someone a role on a scope, replacing the role they had there (needs admin on it)
    Add {
        /// What they may do there (and everywhere inside it)
        #[arg(long, value_enum)]
        role: RoleArg,
        #[command(flatten)]
        who: WhoRequired,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        output: Output,
    },
    /// Remove someone's grant on exactly this scope (needs admin on it)
    Remove {
        #[command(flatten)]
        who: WhoRequired,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        output: Output,
    },
    /// What a user may do, counting their groups, `everyone` and parent scopes (you, by default)
    Check {
        /// The user, by name or uid
        #[arg(long)]
        user: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        #[command(flatten)]
        output: Output,
    },
}

/// Where a grant applies: the whole tree (neither), a supernet, or a subnet.
#[derive(Debug, Args)]
struct ScopeArgs {
    /// A supernet (without it: the whole tree)
    #[arg(long)]
    supernet: Option<Name>,
    /// A subnet in it (needs --supernet)
    #[arg(long, requires = "supernet")]
    subnet: Option<Name>,
}

/// Who a grant is for: exactly one of these.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct WhoRequired {
    /// A user, by name or uid
    #[arg(long)]
    user: Option<String>,
    /// A group, by name or gid
    #[arg(long)]
    group: Option<String>,
    /// Anyone who can connect to the daemon
    #[arg(long)]
    everyone: bool,
}

/// Only grants for one of these (as granted: a user's groups aren't included; see `grant check`).
#[derive(Debug, Args)]
#[group(multiple = false)]
struct WhoFilter {
    /// Only grants to this user (by name or uid)
    #[arg(long)]
    user: Option<String>,
    /// Only grants to this group (by name or gid)
    #[arg(long)]
    group: Option<String>,
    /// Only grants to everyone
    #[arg(long)]
    everyone: bool,
}

/// `--role`
#[derive(Clone, Copy, Debug, ValueEnum)]
enum RoleArg {
    /// Alloc and dealloc ips and hosts
    Use,
    /// …and subnets
    Manage,
    /// …and add and remove grants
    Admin,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Use => Role::Use,
            RoleArg::Manage => Role::Manage,
            RoleArg::Admin => Role::Admin,
        }
    }
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

/// The `resolve` commands' own `--format`: made for scripts.
#[derive(Debug, Args)]
#[command(next_help_heading = EXTRA)]
struct ResolveOutput {
    /// Output format
    #[arg(long, value_enum, default_value_t = ResolveFormat::Simple)]
    format: ResolveFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ResolveFormat {
    /// Just the address (nothing if not found)
    Simple,
    /// `{ supernet, subnet, ip }` side by side (`host resolve`: and `match`), `null` if not found
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

async fn command_supernet_status(
    socket: &Path,
    supernet: Option<Name>,
    block_size: Option<u8>,
) -> Result<(), ObeliskdError> {
    let query = IpQuery {
        supernet,
        ..IpQuery::default()
    };
    let tree = ControlClient::new(socket)?.ips(&query).await?;
    let color = std::io::IsTerminal::is_terminal(&std::io::stdout())
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let out = status::render(&tree, block_size, color).map_err(ObeliskdError::Invalid)?;
    print!("{out}");
    Ok(())
}

async fn run_supernet(socket: &Path, command: SupernetCommand) -> Result<(), ObeliskdError> {
    match command {
        SupernetCommand::List { output } => command_supernet_list(socket, &output).await,
        SupernetCommand::Status {
            supernet,
            block_size,
        } => command_supernet_status(socket, supernet, block_size).await,
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

/// `ip resolve`: 0 found, 1 not found, 2 any other error (printed to stderr).
async fn command_ip_resolve(
    socket: &Path,
    supernet: &Name,
    subnet: &Name,
    ip: &Name,
    output: &ResolveOutput,
) -> ExitCode {
    let found = match ControlClient::new(socket) {
        Ok(client) => client.ip(supernet, subnet, ip).await,
        Err(error) => Err(error),
    };
    print_resolved(found, output, |f| f.ip.address)
}

/// `host resolve`: as `ip resolve`, for a name (wildcards included, as DNS answers it).
async fn command_host_resolve(socket: &Path, host: &Hostname, output: &ResolveOutput) -> ExitCode {
    let found = match ControlClient::new(socket) {
        Ok(client) => client.resolve_host(host).await,
        Err(error) => Err(error),
    };
    print_resolved(found, output, |f| f.ip.address)
}

/// The `resolve` commands' output and exit codes: 0 found, 1 not found (`simple` prints nothing,
/// `json` prints `null`), 2 any other error (on stderr).
fn print_resolved<T: Serialize>(
    found: Result<Option<T>, ObeliskdError>,
    output: &ResolveOutput,
    address: impl FnOnce(&T) -> std::net::Ipv4Addr,
) -> ExitCode {
    match (found, output.format) {
        (Ok(Some(found)), ResolveFormat::Simple) => {
            println!("{}", address(&found));
            ExitCode::SUCCESS
        }
        (Ok(Some(found)), ResolveFormat::Json) => {
            let json = serde_json::to_string_pretty(&found).expect("API types serialize");
            println!("{json}");
            ExitCode::SUCCESS
        }
        (Ok(None), ResolveFormat::Simple) => ExitCode::FAILURE,
        (Ok(None), ResolveFormat::Json) => {
            println!("null");
            ExitCode::FAILURE
        }
        (Err(error), _) => {
            eprintln!("ERROR - {error}");
            ExitCode::from(2)
        }
    }
}

async fn run_ip(socket: &Path, command: IpCommand) -> Result<(), ObeliskdError> {
    match command {
        IpCommand::Resolve { .. } => unreachable!("main_impl runs ip resolve itself"),
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
        HostCommand::Resolve { .. } => unreachable!("main_impl runs host resolve itself"),
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

async fn run_grant(socket: &Path, command: GrantCommand) -> Result<(), ObeliskdError> {
    let client = ControlClient::new(socket)?;
    match command {
        GrantCommand::List { scope, who, output } => {
            let query = GrantQuery {
                supernet: scope.supernet,
                subnet: scope.subnet,
                user: who.user,
                group: who.group,
                everyone: who.everyone,
            };
            let grants = client.grants(&query).await?;
            output.print(&grants, |g| pretty::grants(g));
        }
        GrantCommand::Add {
            role,
            who,
            scope,
            output,
        } => {
            let request = GrantAdd {
                supernet: scope.supernet,
                subnet: scope.subnet,
                who: WhoArg {
                    user: who.user,
                    group: who.group,
                    everyone: who.everyone,
                },
                role: role.into(),
            };
            let grant = client.add_grant(&request).await?;
            output.print(&grant, pretty::grant_added);
        }
        GrantCommand::Remove { who, scope, output } => {
            let query = GrantQuery {
                supernet: scope.supernet,
                subnet: scope.subnet,
                user: who.user,
                group: who.group,
                everyone: who.everyone,
            };
            let grant = client.remove_grant(&query).await?;
            output.print(&grant, pretty::grant_removed);
        }
        GrantCommand::Check {
            user,
            scope,
            output,
        } => {
            let query = GrantCheckQuery {
                user,
                supernet: scope.supernet,
                subnet: scope.subnet,
            };
            let check = client.check_grants(&query).await?;
            output.print(&check, pretty::grant_check);
        }
    }
    Ok(())
}

async fn main_impl() -> Result<ExitCode, ObeliskdError> {
    let cli = Cli::from_arg_matches(&command().get_matches()).unwrap_or_else(|e| e.exit());
    let socket: PathBuf = cli.socket;

    // The commands with exit codes of their own.
    match &cli.command {
        Command::Ip {
            command:
                IpCommand::Resolve {
                    supernet,
                    subnet,
                    ip,
                    output,
                },
        } => return Ok(command_ip_resolve(&socket, supernet, subnet, ip, output).await),
        Command::Host {
            command: HostCommand::Resolve { host, output },
        } => return Ok(command_host_resolve(&socket, host, output).await),
        _ => {}
    }

    match cli.command {
        Command::Daemon { command } => run_daemon(&socket, command).await,
        Command::Supernet { command } => run_supernet(&socket, command).await,
        Command::Subnet { command } => run_subnet(&socket, command).await,
        Command::Ip { command } => run_ip(&socket, command).await,
        Command::Host { command } => run_host(&socket, command).await,
        Command::Grant { command } => run_grant(&socket, command).await,
        Command::Completions { shell } => {
            command_completions(shell);
            Ok(())
        }
    }
    .map(|()| ExitCode::SUCCESS)
}

// Not one worker per core (tokio's default): DNS is a single receive loop and the control API sees a
// handful of requests, so a few workers is plenty, and each one costs a stack and an allocator arena.
#[tokio::main(worker_threads = 4)]
async fn main() -> ExitCode {
    match main_impl().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ERROR - {error}");
            ExitCode::FAILURE
        }
    }
}
