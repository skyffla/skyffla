mod accept_policy;
mod app;
mod cli_error;
mod config;
mod local_state;
mod net;
mod runtime;
mod ui;

use clap::Parser;
use std::process::ExitCode;

use crate::app::commands::{run_host, run_join};
use crate::config::Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_mode = cli.session.json;
    if cli.host {
        match run_host(cli.session).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error.emit(json_mode);
                error.exit_code()
            }
        }
    } else {
        match run_join(cli.session).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                error.emit(json_mode);
                error.exit_code()
            }
        }
    }
}
