use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Parser};

use crate::accept_policy::AutoAcceptPolicy;
use crate::cli_error::CliError;

pub(crate) const DEFAULT_RENDEZVOUS_URL: &str = "http://rendezvous.skyffla.com:8080";

#[derive(Parser)]
#[command(name = "skyffla")]
#[command(about = "Minimal Skyffla peer CLI", long_about = None)]
#[command(version, propagate_version = true)]
pub(crate) struct Cli {
    #[arg(
        short = 'H',
        long,
        help = "Explicitly host the room instead of join-or-promote"
    )]
    pub(crate) host: bool,
    #[command(flatten)]
    pub(crate) session: SessionArgs,
}

#[derive(Args, Clone)]
pub(crate) struct SessionArgs {
    #[arg(env = "SKYFFLA_ROOM_ID")]
    pub(crate) room_id: Option<String>,
    #[arg(
        short = 'm',
        long,
        help = "Use the machine protocol instead of the default TUI"
    )]
    pub(crate) machine: bool,
    #[arg(
        short = 's',
        long,
        help = "Stay online and send this file or folder to each room member once"
    )]
    pub(crate) send: Option<String>,
    #[arg(
        short = 'r',
        long,
        help = "Stay online and auto-accept incoming file or folder transfers"
    )]
    pub(crate) receive: bool,
    #[arg(
        short = 'c',
        long,
        help = "Stay online and send each local clipboard text change to room members"
    )]
    pub(crate) send_clipboard: bool,
    #[arg(
        short = 'C',
        long,
        help = "Stay online and apply incoming clipboard text updates to the local clipboard"
    )]
    pub(crate) receive_clipboard: bool,
    #[arg(
        short = 'S',
        long,
        env = "SKYFFLA_RENDEZVOUS_URL",
        default_value = DEFAULT_RENDEZVOUS_URL
    )]
    pub(crate) server: String,
    #[arg(short = 'd', long, default_value = ".")]
    pub(crate) download_dir: PathBuf,
    #[arg(short = 'n', long, env = "SKYFFLA_NAME")]
    pub(crate) name: Option<String>,
    #[arg(short = 'j', long)]
    pub(crate) json: bool,
    #[arg(short = 'l', long)]
    pub(crate) local: bool,
    #[arg(short = 'a', long, conflicts_with = "reject_all")]
    pub(crate) auto_accept: bool,
    #[arg(short = 'R', long, conflicts_with = "auto_accept")]
    pub(crate) reject_all: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Role {
    Host,
    Join,
}

#[derive(Clone, Debug)]
pub(crate) enum AutomationMode {
    SendPath { path: String },
    ReceivePaths,
    SendClipboard,
    ReceiveClipboard,
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
    pub(crate) auto_accept_policy: AutoAcceptPolicy,
    pub(crate) automation: Option<AutomationMode>,
}

impl SessionConfig {
    pub(crate) fn from_args(role: Role, args: SessionArgs) -> Result<Self, CliError> {
        let automation = resolve_automation_mode(&args)?;
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
                std::env::var("SKYFFLA_NAME").ok(),
                std::env::var("USER").ok(),
                std::env::var("USERNAME").ok(),
            ),
            machine: args.machine,
            json_events: args.json,
            local_mode: args.local,
            auto_accept_policy: resolve_auto_accept_policy(args.auto_accept, args.reject_all).0,
            automation,
        })
    }
}

fn resolve_automation_mode(args: &SessionArgs) -> Result<Option<AutomationMode>, CliError> {
    let selected_modes = [
        args.send.is_some(),
        args.receive,
        args.send_clipboard,
        args.receive_clipboard,
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count();
    if selected_modes > 1 {
        return Err(CliError::usage(
            "--send, --receive, --send-clipboard, and --receive-clipboard are mutually exclusive",
        ));
    }
    if selected_modes == 0 {
        return Ok(None);
    }

    if args.machine {
        return Err(CliError::usage(
            "--send/--receive/--send-clipboard/--receive-clipboard already use the machine runtime; do not combine them with --machine",
        ));
    }
    if args.json {
        return Err(CliError::usage(
            "--send/--receive/--send-clipboard/--receive-clipboard provide human-readable CLI logs; do not combine them with --json",
        ));
    }
    if args.auto_accept || args.reject_all {
        return Err(CliError::usage(
            "--send/--receive/--send-clipboard/--receive-clipboard manage transfer acceptance automatically; do not combine them with --auto-accept or --reject-all",
        ));
    }

    Ok(
        match (
            &args.send,
            args.receive,
            args.send_clipboard,
            args.receive_clipboard,
        ) {
            (Some(path), false, false, false) => {
                Some(AutomationMode::SendPath { path: path.clone() })
            }
            (None, true, false, false) => Some(AutomationMode::ReceivePaths),
            (None, false, true, false) => Some(AutomationMode::SendClipboard),
            (None, false, false, true) => Some(AutomationMode::ReceiveClipboard),
            _ => None,
        },
    )
}

fn resolve_room_id(explicit: Option<String>, env_value: Option<String>) -> Option<String> {
    explicit
        .or(env_value)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_peer_name(
    explicit: Option<String>,
    skyffla_name_env: Option<String>,
    user_env: Option<String>,
    username_env: Option<String>,
) -> String {
    [explicit, skyffla_name_env, user_env, username_env]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .next()
        .unwrap_or_else(|| "skyffla-peer".into())
}

fn resolve_auto_accept_policy(
    auto_accept: bool,
    reject_all: bool,
) -> (AutoAcceptPolicy, &'static str) {
    if reject_all {
        return (AutoAcceptPolicy::none(), "cli override");
    }
    if auto_accept {
        return (AutoAcceptPolicy::all(), "cli override");
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
    use std::path::PathBuf;

    use super::{
        resolve_auto_accept_policy, resolve_automation_mode, resolve_peer_name, resolve_room_id,
        validate_room_id, AutomationMode, Cli, SessionArgs, DEFAULT_RENDEZVOUS_URL,
    };
    use crate::accept_policy::AutoAcceptPolicy;

    fn session_args() -> SessionArgs {
        SessionArgs {
            room_id: Some("room".into()),
            machine: false,
            send: None,
            receive: false,
            send_clipboard: false,
            receive_clipboard: false,
            server: DEFAULT_RENDEZVOUS_URL.into(),
            download_dir: ".".into(),
            name: None,
            json: false,
            local: false,
            auto_accept: false,
            reject_all: false,
        }
    }

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
            resolve_peer_name(
                Some("cli-name".into()),
                Some("skyffla-name".into()),
                Some("user-name".into()),
                None
            ),
            "cli-name"
        );
        assert_eq!(
            resolve_peer_name(
                None,
                Some("skyffla-name".into()),
                Some("user-name".into()),
                Some("username-name".into())
            ),
            "skyffla-name"
        );
        assert_eq!(
            resolve_peer_name(
                None,
                Some("   ".into()),
                Some("user-name".into()),
                Some("username-name".into())
            ),
            "user-name"
        );
        assert_eq!(
            resolve_peer_name(None, None, None, Some("username-name".into())),
            "username-name"
        );
        assert_eq!(resolve_peer_name(None, None, None, None), "skyffla-peer");
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
    fn cli_auto_accept_accepts_everything() {
        let (policy, source) = resolve_auto_accept_policy(true, false);

        assert_eq!(policy, AutoAcceptPolicy::all());
        assert_eq!(source, "cli override");
    }

    #[test]
    fn auto_accept_flag_takes_no_values() {
        let cli = <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "--auto-accept"])
            .expect("--auto-accept should parse as a flag");
        assert!(cli.session.auto_accept);

        let cli = <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-a"])
            .expect("-a should parse as --auto-accept");
        assert!(cli.session.auto_accept);

        assert!(<Cli as clap::Parser>::try_parse_from([
            "skyffla",
            "room",
            "--auto-accept",
            "folder"
        ])
        .is_err());

        assert!(
            <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-a", "folder"]).is_err()
        );
    }

    #[test]
    fn short_options_match_long_options() {
        let cli = <Cli as clap::Parser>::try_parse_from([
            "skyffla",
            "room",
            "-H",
            "-m",
            "-j",
            "-l",
            "-n",
            "beta",
            "-S",
            "http://127.0.0.1:8080",
            "-d",
            "downloads",
            "-R",
        ])
        .expect("short options should parse");

        assert!(cli.host);
        assert!(cli.session.machine);
        assert!(cli.session.json);
        assert!(cli.session.local);
        assert_eq!(cli.session.name.as_deref(), Some("beta"));
        assert_eq!(cli.session.server, "http://127.0.0.1:8080");
        assert_eq!(cli.session.download_dir, PathBuf::from("downloads"));
        assert!(cli.session.reject_all);
    }

    #[test]
    fn automation_short_options_match_long_options() {
        let send_cli = <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-s", "file.txt"])
            .expect("-s should parse as --send");
        assert_eq!(send_cli.session.send.as_deref(), Some("file.txt"));

        let receive_cli = <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-r"])
            .expect("-r should parse as --receive");
        assert!(receive_cli.session.receive);

        let send_clipboard_cli = <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-c"])
            .expect("-c should parse as --send-clipboard");
        assert!(send_clipboard_cli.session.send_clipboard);

        let receive_clipboard_cli =
            <Cli as clap::Parser>::try_parse_from(["skyffla", "room", "-C"])
                .expect("-C should parse as --receive-clipboard");
        assert!(receive_clipboard_cli.session.receive_clipboard);
    }

    #[test]
    fn reject_all_disables_acceptance() {
        let (policy, source) = resolve_auto_accept_policy(false, true);

        assert_eq!(policy, AutoAcceptPolicy::none());
        assert_eq!(source, "cli override");
    }

    #[test]
    fn default_policy_accepts_nothing() {
        let (policy, source) = resolve_auto_accept_policy(false, false);

        assert_eq!(policy, AutoAcceptPolicy::none());
        assert_eq!(source, "default");
    }

    #[test]
    fn send_mode_rejects_machine_and_json_flags() {
        let mut args = session_args();
        args.send = Some("./report.txt".into());
        args.machine = true;
        assert!(resolve_automation_mode(&args).is_err());

        args.machine = false;
        args.json = true;
        assert!(resolve_automation_mode(&args).is_err());
    }

    #[test]
    fn receive_mode_is_selected() {
        let mut args = session_args();
        args.receive = true;
        assert!(matches!(
            resolve_automation_mode(&args).unwrap(),
            Some(AutomationMode::ReceivePaths)
        ));
    }

    #[test]
    fn send_mode_preserves_raw_path_text() {
        let mut args = session_args();
        args.send = Some("~/report.txt".into());
        assert!(matches!(
            resolve_automation_mode(&args).unwrap(),
            Some(AutomationMode::SendPath { path }) if path == "~/report.txt"
        ));
    }

    #[test]
    fn clipboard_modes_are_selected() {
        let mut send_args = session_args();
        send_args.send_clipboard = true;
        assert!(matches!(
            resolve_automation_mode(&send_args).unwrap(),
            Some(AutomationMode::SendClipboard)
        ));

        let mut receive_args = session_args();
        receive_args.receive_clipboard = true;
        assert!(matches!(
            resolve_automation_mode(&receive_args).unwrap(),
            Some(AutomationMode::ReceiveClipboard)
        ));
    }

    #[test]
    fn clipboard_modes_are_mutually_exclusive_with_existing_automation() {
        let mut args = session_args();
        args.send = Some("./report.txt".into());
        args.send_clipboard = true;
        assert!(resolve_automation_mode(&args).is_err());

        args.send = None;
        args.send_clipboard = false;
        args.receive = true;
        args.receive_clipboard = true;
        assert!(resolve_automation_mode(&args).is_err());
    }
}
