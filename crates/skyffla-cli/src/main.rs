mod accept_policy;
mod app;
mod config;
mod local_state;
mod net;
mod runtime;
mod transfers;
mod ui;

use anyhow::Result;
use clap::Parser;

use crate::app::commands::{run_host, run_join};
use crate::config::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Host(args) => run_host(args).await,
        Command::Join(args) => run_join(args).await,
    }
}
