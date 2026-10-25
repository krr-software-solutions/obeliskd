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
    /// Print a greeting
    Hello {
        /// Fail with an error instead of printing
        #[arg(long)]
        fail: bool,
    },
    /// Print a shell completion script
    Completions { shell: clap_complete::Shell },
}

//

#[expect(clippy::unused_async, reason = "placeholder: real commands will await")]
async fn command_hello(fail: bool) -> Result<(), ObeliskdError> {
    obeliskd::print_hello(fail)
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
        Command::Hello { fail } => command_hello(fail).await,
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
