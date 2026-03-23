use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Parser};

#[cfg(test)]
use crate::accept_policy::AutoAcceptPolicy;
use crate::accept_policy::AutoAcceptTarget;
use crate::cli_error::CliError;

pub(crate) const DEFAULT_RENDEZVOUS_URL: &str = "http://rendezvous.skyffla.com:8080";

#[derive(Parser)]
#[command(name = "skyffla")]
#[command(about = "Minimal Skyffla peer CLI", long_about = None)]
#[command(version, propagate_version = true)]
pub(crate) struct Cli {
    #[arg(long, help = "Explicitly host the room instead of join-or-promote")]
    pub(crate) host: bool,
    #[command(flatten)]
    pub(crate) session: SessionArgs,
}

#[derive(Args, Clone)]
pub(crate) struct SessionArgs {
    pub(crate) room_id: Option<String>,
    #[arg(long, help = "Use the machine protocol instead of the default TUI")]
    pub(crate) machine: bool,
    #[arg(
        long,
        env = "SKYFFLA_RENDEZVOUS_URL",
        default_value = DEFAULT_RENDEZVOUS_URL
    )]
    pub(crate) server: String,
    #[arg(long, default_value = ".")]
    pub(crate) download_dir: PathBuf,
    #[arg(long)]
    pub(crate) name: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
    #[arg(long)]
    pub(crate) local: bool,
    #[arg(long, value_enum, value_delimiter = ',', conflicts_with = "reject_all")]
    pub(crate) auto_accept: Vec<AutoAcceptTarget>,
    #[arg(long, conflicts_with = "auto_accept")]
    pub(crate) reject_all: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Host,
    Join,
}

#[derive(Clone)]
pub(crate) struct SessionConfig {
    pub(crate) role: Role,
    pub(crate) room_id: String,
    pub(crate) rendezvous_server: String,
    pub(crate) download_dir: PathBuf,
    pub(crate) peer_name: String,
    pub(crate) machine: bool,
    pub(crate) json_events: bool,
    pub(crate) local_mode: bool,
}

impl SessionConfig {
    pub(crate) fn from_args(role: Role, args: SessionArgs) -> Result<Self, CliError> {
        let room_id = resolve_room_id(args.room_id, std::env::var("SKYFFLA_ROOM_ID").ok())
            .ok_or_else(|| {
                CliError::usage("missing room id: pass it as an argument or set SKYFFLA_ROOM_ID")
            })?;
        validate_room_id(&room_id).map_err(|error| CliError::usage(error.to_string()))?;
        Ok(Self {
            role,
            room_id,
            rendezvous_server: args.server,
            download_dir: args.download_dir,
            peer_name: resolve_peer_name(
                args.name,
                std::env::var("USER").ok(),
                std::env::var("USERNAME").ok(),
            ),
            machine: args.machine,
            json_events: args.json,
            local_mode: args.local,
        })
    }
}

fn resolve_room_id(explicit: Option<String>, env_value: Option<String>) -> Option<String> {
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

#[cfg(test)]
fn resolve_auto_accept_policy(
    persisted: &AutoAcceptPolicy,
    cli_targets: &[AutoAcceptTarget],
    reject_all: bool,
) -> (AutoAcceptPolicy, &'static str) {
    if reject_all {
        return (AutoAcceptPolicy::none(), "cli override");
    }
    if !cli_targets.is_empty() {
        return (AutoAcceptPolicy::from_targets(cli_targets), "cli override");
    }
    if !persisted.is_empty() {
        return (persisted.clone(), "local state");
    }
    (AutoAcceptPolicy::none(), "default")
}

fn validate_room_id(room_id: &str) -> Result<()> {
    if room_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Ok(());
    }

    bail!(
        "invalid room id {:?}: use only ASCII letters, digits, '-' or '_'",
        room_id
    );
}

#[cfg(test)]
mod tests {
    use super::{resolve_auto_accept_policy, resolve_peer_name, resolve_room_id, validate_room_id};
    use crate::accept_policy::{AutoAcceptPolicy, AutoAcceptTarget};

    #[test]
    fn explicit_room_id_wins_over_environment() {
        let room_id = resolve_room_id(Some("cli-room".into()), Some("env-room".into()));
        assert_eq!(room_id.as_deref(), Some("cli-room"));
    }

    #[test]
    fn blank_room_ids_are_treated_as_missing() {
        assert_eq!(resolve_room_id(Some("   ".into()), None), None);
        assert_eq!(resolve_room_id(None, Some(" \n ".into())), None);
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
    fn room_id_rejects_special_characters() {
        assert!(validate_room_id("copper-731").is_ok());
        assert!(validate_room_id("copper_731").is_ok());
        assert!(validate_room_id("copper/731").is_err());
        assert!(validate_room_id("copper 731").is_err());
        assert!(validate_room_id("copper?731").is_err());
    }

    #[test]
    fn cli_auto_accept_overrides_persisted_policy() {
        let persisted = AutoAcceptPolicy::files_and_clipboard();
        let (policy, source) =
            resolve_auto_accept_policy(&persisted, &[AutoAcceptTarget::Folder], false);

        assert_eq!(
            policy,
            AutoAcceptPolicy {
                file: false,
                folder: true,
                clipboard: false,
            }
        );
        assert_eq!(source, "cli override");
    }

    #[test]
    fn reject_all_disables_any_persisted_acceptance() {
        let persisted = AutoAcceptPolicy::files_and_clipboard();
        let (policy, source) = resolve_auto_accept_policy(&persisted, &[], true);

        assert_eq!(policy, AutoAcceptPolicy::none());
        assert_eq!(source, "cli override");
    }

    #[test]
    fn persisted_policy_is_used_when_no_cli_override_exists() {
        let persisted = AutoAcceptPolicy::files_and_clipboard();
        let (policy, source) = resolve_auto_accept_policy(&persisted, &[], false);

        assert_eq!(policy, persisted);
        assert_eq!(source, "local state");
    }
}
