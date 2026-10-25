use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use obeliskd::control::client::ControlClient;
use obeliskd::{ObeliskdError, Platform, PlatformImpl};

//

/// A small, hand-written DNS server.
#[derive(Debug, Parser)]
#[command(name = "obeliskd", version)]
struct Cli {
    /// The daemon's control socket [default: /run/obeliskd/obeliskd.sock on Linux]
    #[arg(long, global = true, env = "OBELISKD_SOCKET")]
    socket: Option<PathBuf>,

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
    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground, serving the control socket
    Run,
    /// Ask the running daemon how it's doing, and who it sees you as
    Health,
}

//

async fn command_daemon_run(socket: &Path) -> Result<(), ObeliskdError> {
    obeliskd::control::server::run(socket).await
}

async fn command_daemon_health(socket: &Path) -> Result<(), ObeliskdError> {
    let health = ControlClient::new(socket)?.health().await?;
    let json = serde_json::to_string_pretty(&health).expect("Health serializes");
    println!("{json}");
    Ok(())
}

fn command_completions(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
}

//

async fn main_impl() -> Result<(), ObeliskdError> {
    let cli: Cli = Cli::parse();
    let socket: PathBuf = cli.socket.unwrap_or_else(Platform::control_socket_path);

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Run => command_daemon_run(&socket).await,
            DaemonCommand::Health => command_daemon_health(&socket).await,
        },
        Command::Completions { shell } => {
            command_completions(shell);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match main_impl().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR - {error}");
            ExitCode::FAILURE
        }
    }
}
