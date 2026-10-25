use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use obeliskd::ObeliskdError;

//

/// A small, hand-written DNS server.
#[derive(Debug, Parser)]
#[command(name = "obeliskd", version)]
struct Cli {
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
    /// Run the daemon in the foreground
    Run,
}

//

#[expect(clippy::unused_async, reason = "placeholder: real commands will await")]
async fn command_daemon_run() -> Result<(), ObeliskdError> {
    println!("todo");
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

    match cli.command {
        Command::Daemon { command } => match command {
            DaemonCommand::Run => command_daemon_run().await,
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
