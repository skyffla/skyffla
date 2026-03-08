use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "skyffla")]
#[command(about = "Minimal Skyffla peer CLI", long_about = None)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Host(SessionArgs),
    Join(SessionArgs),
}

#[derive(Args, Clone)]
pub(crate) struct SessionArgs {
    pub(crate) stream_id: Option<String>,
    #[arg(
        long,
        env = "SKYFFLA_RENDEZVOUS_URL",
        default_value = "http://127.0.0.1:8080"
    )]
    pub(crate) server: String,
    #[arg(long, default_value = ".")]
    pub(crate) download_dir: PathBuf,
    #[arg(long)]
    pub(crate) name: Option<String>,
    #[arg(long)]
    pub(crate) message: Option<String>,
    #[arg(long)]
    pub(crate) stdio: bool,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Host,
    Join,
}

pub(crate) struct SessionConfig {
    pub(crate) role: Role,
    pub(crate) stream_id: String,
    pub(crate) rendezvous_server: String,
    pub(crate) download_dir: PathBuf,
    pub(crate) peer_name: String,
    pub(crate) outgoing_message: Option<String>,
    pub(crate) stdio: bool,
    pub(crate) json_events: bool,
}

impl SessionConfig {
    pub(crate) fn from_args(role: Role, args: SessionArgs) -> Self {
        validate_stdio_message_flags(args.stdio, args.message.as_deref()).unwrap_or_else(|error| {
            eprintln!("{error}");
            std::process::exit(2);
        });
        let stream_id = resolve_stream_id(args.stream_id, std::env::var("SKYFFLA_STREAM_ID").ok())
            .unwrap_or_else(|| {
                eprintln!("missing stream id: pass it as an argument or set SKYFFLA_STREAM_ID");
                std::process::exit(2);
            });
        Self {
            role,
            stream_id,
            rendezvous_server: args.server,
            download_dir: args.download_dir,
            peer_name: resolve_peer_name(
                args.name,
                std::env::var("USER").ok(),
                std::env::var("USERNAME").ok(),
            ),
            outgoing_message: args.message,
            stdio: args.stdio,
            json_events: args.json,
        }
    }
}

fn resolve_stream_id(explicit: Option<String>, env_value: Option<String>) -> Option<String> {
    explicit
        .or(env_value)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_peer_name(
    explicit: Option<String>,
    user_env: Option<String>,
    username_env: Option<String>,
) -> String {
    explicit
        .or(user_env)
        .or(username_env)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "skyffla-peer".into())
}

fn validate_stdio_message_flags(stdio: bool, message: Option<&str>) -> Result<()> {
    if stdio && message.is_some() {
        bail!("--stdio cannot be combined with --message");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{resolve_peer_name, resolve_stream_id, validate_stdio_message_flags};

    #[test]
    fn explicit_stream_id_wins_over_environment() {
        let stream_id = resolve_stream_id(Some("cli-room".into()), Some("env-room".into()));
        assert_eq!(stream_id.as_deref(), Some("cli-room"));
    }

    #[test]
    fn blank_stream_ids_are_treated_as_missing() {
        assert_eq!(resolve_stream_id(Some("   ".into()), None), None);
        assert_eq!(resolve_stream_id(None, Some(" \n ".into())), None);
    }

    #[test]
    fn peer_name_falls_back_through_expected_sources() {
        assert_eq!(
            resolve_peer_name(Some("cli-name".into()), Some("user-name".into()), None),
            "cli-name"
        );
        assert_eq!(
            resolve_peer_name(None, Some("user-name".into()), Some("username-name".into())),
            "user-name"
        );
        assert_eq!(
            resolve_peer_name(None, None, Some("username-name".into())),
            "username-name"
        );
        assert_eq!(resolve_peer_name(None, None, None), "skyffla-peer");
    }

    #[test]
    fn stdio_and_message_are_mutually_exclusive() {
        assert!(validate_stdio_message_flags(true, Some("hello")).is_err());
        assert!(validate_stdio_message_flags(false, Some("hello")).is_ok());
        assert!(validate_stdio_message_flags(true, None).is_ok());
    }
}
