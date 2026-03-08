mod accept_policy;
mod app;
mod cli_error;
mod config;
mod local_state;
mod net;
mod runtime;
mod transfers;
mod ui;

use clap::Parser;
use std::process::ExitCode;

use crate::app::commands::{run_host, run_join};
use crate::config::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_mode = match &cli.command {
        Command::Host(args) | Command::Join(args) => args.json,
    };
    match cli.command {
        Command::Host(args) => match run_host(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error.emit(json_mode);
                error.exit_code()
            }
        },
        Command::Join(args) => match run_join(args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error.emit(json_mode);
                error.exit_code()
            }
        },
    }
}
