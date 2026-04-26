use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::future::pending;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::Value;
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, MemberId, Route, TransferItemKind,
    TransferPhase,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::cli_error::CliError;
use crate::config::{AutomationMode, ReceiveOutput, Role, SessionConfig};
use crate::runtime::clipboard::{ClipboardBackend, ClipboardEvent, ClipboardMode};

pub(crate) async fn run_transfer_automation(
    role: Role,
    config: &SessionConfig,
) -> Result<(), CliError> {
    let mode = config
        .automation
        .clone()
        .ok_or_else(|| CliError::runtime("transfer automation mode is missing"))?;
    let mut state = AutomationState::new(config, mode)?;
    let mut backend = spawn_machine_backend(role, config).await?;
    let mut clipboard = spawn_clipboard_backend(&state)?;
    let mut last_backend_status = None;
    let mut log = LogSink::new(state.logs_to_stderr(), config.quiet);

    log.info(&state.startup_line());

    loop {
        tokio::select! {
            maybe_clipboard = async {
                if let Some(backend) = clipboard.as_mut() {
                    backend.recv_event().await
                } else {
                    pending::<Option<ClipboardEvent>>().await
                }
            } => {
                let Some(event) = maybe_clipboard else {
                    clipboard = None;
                    continue;
                };
                handle_clipboard_event(&mut state, &mut backend.stdin, clipboard.as_ref(), &mut log, event).await?;
            }
            maybe_event = backend.stdout_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                handle_machine_event(&mut state, &mut backend.stdin, clipboard.as_ref(), &mut log, event).await?;
            }
            maybe_status = backend.stderr_rx.recv() => {
                let Some(line) = maybe_status else {
                    continue;
                };
                if let Some(message) = backend_status_line(&line) {
                    last_backend_status = Some(message.clone());
                    log.info(&message);
                } else if serde_json::from_str::<Value>(&line).is_ok() {
                    continue;
                } else if !line.trim().is_empty() {
                    last_backend_status = Some(line.clone());
                    log.info(&line);
                }
            }
            status = backend.child.wait() => {
                let status = status.map_err(|error| CliError::runtime(error.to_string()))?;
                drain_backend_status_lines(&mut backend.stderr_rx, &mut last_backend_status);
                log.finish_status();
                if status.success() {
                    return Ok(());
                }
                return Err(backend_exit_error(status, last_backend_status));
            }
        }
    }

    let status = backend
        .child
        .wait()
        .await
        .map_err(|error| CliError::runtime(error.to_string()))?;
    drain_backend_status_lines(&mut backend.stderr_rx, &mut last_backend_status);
    log.finish_status();
    if status.success() {
        Ok(())
    } else {
        Err(backend_exit_error(status, last_backend_status))
    }
}

enum LogTarget {
    Stdout(io::Stdout),
    Stderr(io::Stderr),
}

impl LogTarget {
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.write_fmt(args),
            Self::Stderr(stderr) => stderr.write_fmt(args),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.flush(),
            Self::Stderr(stderr) => stderr.flush(),
        }
    }
}

struct LogSink {
    target: LogTarget,
    interactive: bool,
    status_active: bool,
    quiet: bool,
}

impl LogSink {
    fn new(stderr: bool, quiet: bool) -> Self {
        if stderr {
            return Self {
                target: LogTarget::Stderr(io::stderr()),
                interactive: io::stderr().is_terminal(),
                status_active: false,
                quiet,
            };
        }
        Self {
            target: LogTarget::Stdout(io::stdout()),
            interactive: io::stdout().is_terminal(),
            status_active: false,
            quiet,
        }
    }

    fn info(&mut self, message: &str) {
        if self.quiet {
            return;
        }
        let line = format!("[{}] {message}", timestamp_now());
        if self.interactive && self.status_active {
            let _ = self.target.write_fmt(format_args!("\r\x1b[2K"));
            self.status_active = false;
        }
        let _ = self.target.write_fmt(format_args!("{line}\n"));
        let _ = self.target.flush();
    }

    fn status(&mut self, message: &str) {
        if self.quiet {
            return;
        }
        if !self.interactive {
            self.info(message);
            return;
        }
        let line = format!("[{}] {message}", timestamp_now());
        let _ = self.target.write_fmt(format_args!("\r\x1b[2K{line}"));
        let _ = self.target.flush();
        self.status_active = true;
    }

    fn finish_status(&mut self) {
        if self.quiet {
            return;
        }
        if self.interactive && self.status_active {
            let _ = self.target.write_fmt(format_args!("\n"));
            let _ = self.target.flush();
            self.status_active = false;
        }
    }
}

struct AutomationBackend {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_rx: mpsc::UnboundedReceiver<MachineEvent>,
    stderr_rx: mpsc::UnboundedReceiver<String>,
}

async fn spawn_machine_backend(
    role: Role,
    config: &SessionConfig,
) -> Result<AutomationBackend, CliError> {
    let exe = std::env::current_exe().map_err(|error| CliError::local_io(error.to_string()))?;
    let mut command = Command::new(exe);
    command.arg(&config.room_id);
    if matches!(role, Role::Host) {
        command.arg("--host");
    }
    command.arg("--machine");
    command.arg("--json");
    command.arg("--server").arg(&config.rendezvous_server);
    command.arg("--download-dir").arg(&config.download_dir);
    command.arg("--name").arg(&config.peer_name);
    if config.local_mode {
        command.arg("--local");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .context("failed to spawn transfer automation backend")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    let stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::runtime("automation backend stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CliError::runtime("automation backend stderr missing"))?;

    let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
    let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
    spawn_machine_stdout_reader(stdout, stdout_tx);
    spawn_machine_stderr_reader(stderr, stderr_tx);

    Ok(AutomationBackend {
        child,
        stdin,
        stdout_rx,
        stderr_rx,
    })
}

fn spawn_machine_stdout_reader(stdout: ChildStdout, tx: mpsc::UnboundedSender<MachineEvent>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(event) = serde_json::from_str::<MachineEvent>(&line) {
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
    });
}

fn spawn_machine_stderr_reader(stderr: ChildStderr, tx: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

#[derive(Clone)]
struct SendSource {
    raw_path: String,
    display_name: String,
    item_kind: TransferItemKind,
    kind: SendSourceKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendSourceKind {
    Path,
    Stdin,
}

enum ControllerMode {
    SendPath { source: SendSource },
    ReceivePaths { output: ReceiveOutput },
    SendClipboard,
    ReceiveClipboard,
}

#[derive(Clone)]
enum AutomationItemKind {
    Transfer(TransferItemKind),
    Clipboard,
}

struct AutomationState {
    room_id: String,
    mode: ControllerMode,
    next_channel_index: u64,
    self_member: Option<MemberId>,
    host_member: Option<MemberId>,
    members: BTreeMap<MemberId, String>,
    sent_members: BTreeSet<MemberId>,
    pending_offers: BTreeMap<MemberId, ChannelId>,
    channels: BTreeMap<ChannelId, ChannelState>,
    latest_clipboard_text: Option<String>,
    stdin_spool_path: Option<PathBuf>,
    one_shot_targets_closed: bool,
    intentional_leave: bool,
    stdout_receive_completed: bool,
}

struct ChannelState {
    item_kind: AutomationItemKind,
    name: String,
    direction: ChannelDirection,
    progress: Option<ProgressState>,
    accepted: bool,
}

struct ProgressState {
    phase: TransferPhase,
    started_at: Instant,
    bytes_complete: u64,
    bytes_total: Option<u64>,
}

enum ChannelDirection {
    Outgoing {
        member_id: MemberId,
    },
    Incoming {
        member_id: MemberId,
        auto_accept: bool,
    },
}

impl AutomationState {
    fn new(config: &SessionConfig, automation: AutomationMode) -> Result<Self, CliError> {
        let mut stdin_spool_path = None;
        let mode = match automation {
            AutomationMode::SendPath { path, display_name } => {
                let (source, spool_path) = resolve_send_source(&path, display_name)?;
                stdin_spool_path = spool_path;
                ControllerMode::SendPath { source }
            }
            AutomationMode::ReceivePaths { output } => ControllerMode::ReceivePaths { output },
            AutomationMode::SendClipboard => ControllerMode::SendClipboard,
            AutomationMode::ReceiveClipboard => ControllerMode::ReceiveClipboard,
            AutomationMode::Pipe { .. } => {
                return Err(CliError::runtime("pipe mode uses the native pipe runtime"));
            }
        };
        Ok(Self {
            room_id: config.room_id.clone(),
            mode,
            next_channel_index: 1,
            self_member: None,
            host_member: None,
            members: BTreeMap::new(),
            sent_members: BTreeSet::new(),
            pending_offers: BTreeMap::new(),
            channels: BTreeMap::new(),
            latest_clipboard_text: None,
            stdin_spool_path,
            one_shot_targets_closed: false,
            intentional_leave: false,
            stdout_receive_completed: false,
        })
    }

    fn startup_line(&self) -> String {
        match &self.mode {
            ControllerMode::SendPath { source } => {
                if source.kind == SendSourceKind::Stdin {
                    format!(
                        "send stdin mode: waiting for peers in room {} and offering {} {}",
                        self.room_id,
                        transfer_item_kind_label(&source.item_kind),
                        source.display_name
                    )
                } else {
                    format!(
                        "send mode: staying online in room {} and offering {} {} to each member once",
                        self.room_id,
                        transfer_item_kind_label(&source.item_kind),
                        source.display_name
                    )
                }
            }
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::DownloadDir,
            } => format!(
                "receive mode: staying online in room {} and auto-accepting incoming transfers",
                self.room_id
            ),
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout,
            } => format!(
                "receive stdout mode: staying online in room {} and writing one incoming file to stdout",
                self.room_id
            ),
            ControllerMode::SendClipboard => format!(
                "send clipboard mode: staying online in room {} and forwarding local clipboard text to accepted peers",
                self.room_id
            ),
            ControllerMode::ReceiveClipboard => format!(
                "receive clipboard mode: staying online in room {} and applying incoming clipboard text updates",
                self.room_id
            ),
        }
    }

    fn local_member_id(&self) -> Option<&MemberId> {
        self.self_member.as_ref()
    }

    fn display_member(&self, member_id: &MemberId) -> String {
        self.members
            .get(member_id)
            .map(|name| {
                if self
                    .members
                    .values()
                    .filter(|candidate| candidate.as_str() == name.as_str())
                    .take(2)
                    .count()
                    > 1
                {
                    format!("{name} ({})", member_id.as_str())
                } else {
                    name.clone()
                }
            })
            .unwrap_or_else(|| member_id.as_str().to_string())
    }

    fn next_channel_id(&mut self) -> ChannelId {
        let channel_id = ChannelId::new(format!("auto-{}", self.next_channel_index))
            .expect("generated automation channel ids should be valid");
        self.next_channel_index += 1;
        channel_id
    }

    fn record_progress(
        &mut self,
        channel_id: &ChannelId,
        phase: &TransferPhase,
        bytes_complete: u64,
        bytes_total: Option<u64>,
    ) -> Option<&ProgressState> {
        let channel = self.channels.get_mut(channel_id)?;
        match channel.progress.as_mut() {
            Some(progress) if progress_phase_rank(&progress.phase) > progress_phase_rank(phase) => {
                return channel.progress.as_ref();
            }
            Some(progress) if progress.phase == *phase => {
                progress.bytes_complete = progress.bytes_complete.max(bytes_complete);
                progress.bytes_total = match (progress.bytes_total, bytes_total) {
                    (Some(existing), Some(incoming)) => Some(existing.max(incoming)),
                    (Some(existing), None) => Some(existing),
                    (None, Some(incoming)) => Some(incoming),
                    (None, None) => None,
                };
            }
            _ => {
                channel.progress = Some(ProgressState {
                    phase: phase.clone(),
                    started_at: Instant::now(),
                    bytes_complete,
                    bytes_total,
                });
            }
        }
        channel.progress.as_ref()
    }

    fn clear_member_channel(&mut self, member_id: &MemberId, channel_id: &ChannelId) {
        if self
            .pending_offers
            .get(member_id)
            .is_some_and(|value| value == channel_id)
        {
            self.pending_offers.remove(member_id);
        }
    }

    fn allows_incoming_clipboard(&self) -> bool {
        matches!(self.mode, ControllerMode::ReceiveClipboard)
    }

    fn is_send_clipboard_mode(&self) -> bool {
        matches!(self.mode, ControllerMode::SendClipboard)
    }

    fn logs_to_stderr(&self) -> bool {
        matches!(
            self.mode,
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout
            }
        )
    }

    fn should_auto_accept_transfer(&self, item_kind: &TransferItemKind) -> bool {
        match &self.mode {
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::DownloadDir,
            } => true,
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout,
            } => matches!(item_kind, TransferItemKind::File) && !self.stdout_receive_completed,
            _ => false,
        }
    }

    fn transfer_rejection_reason(&self, item_kind: &TransferItemKind) -> &'static str {
        match &self.mode {
            ControllerMode::SendPath { .. } => "peer is in send mode",
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::DownloadDir,
            } => "",
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout,
            } if matches!(item_kind, TransferItemKind::Folder) => {
                "stdout receive mode only accepts file transfers"
            }
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout,
            } => "stdout receive mode already received one file",
            ControllerMode::SendClipboard => "peer is in clipboard send mode",
            ControllerMode::ReceiveClipboard => "peer is in clipboard receive mode",
        }
    }

    fn should_write_receive_to_stdout(&self) -> bool {
        matches!(
            self.mode,
            ControllerMode::ReceivePaths {
                output: ReceiveOutput::Stdout
            }
        )
    }

    fn is_one_shot_send(&self) -> bool {
        matches!(
            self.mode,
            ControllerMode::SendPath {
                source: SendSource {
                    kind: SendSourceKind::Stdin,
                    ..
                }
            }
        )
    }

    fn close_one_shot_targets_if_started(&mut self) {
        if self.is_one_shot_send() && !self.sent_members.is_empty() {
            self.one_shot_targets_closed = true;
        }
    }

    fn should_finish_one_shot_send(&self) -> bool {
        self.is_one_shot_send()
            && self.one_shot_targets_closed
            && !self.sent_members.is_empty()
            && self.pending_offers.is_empty()
            && !self.channels.values().any(|channel| {
                matches!(channel.direction, ChannelDirection::Outgoing { .. })
                    && matches!(channel.item_kind, AutomationItemKind::Transfer(_))
            })
    }

    fn has_outgoing_clipboard_channel(&self, member_id: &MemberId) -> bool {
        self.channels.values().any(|channel| {
            matches!(
                (&channel.item_kind, &channel.direction),
                (
                    AutomationItemKind::Clipboard,
                    ChannelDirection::Outgoing { member_id: peer_id }
                ) if peer_id == member_id
            )
        })
    }

    fn accepted_clipboard_channels(&self) -> Vec<(ChannelId, MemberId)> {
        self.channels
            .iter()
            .filter_map(
                |(channel_id, channel)| match (&channel.item_kind, &channel.direction) {
                    (AutomationItemKind::Clipboard, ChannelDirection::Outgoing { member_id })
                        if channel.accepted =>
                    {
                        Some((channel_id.clone(), member_id.clone()))
                    }
                    _ => None,
                },
            )
            .collect()
    }
}

impl Drop for AutomationState {
    fn drop(&mut self) {
        if let Some(path) = &self.stdin_spool_path {
            let _ = fs::remove_file(path);
        }
    }
}

fn progress_phase_rank(phase: &TransferPhase) -> u8 {
    match phase {
        TransferPhase::Preparing => 0,
        TransferPhase::Downloading => 1,
        TransferPhase::Exporting => 2,
    }
}

fn spawn_clipboard_backend(state: &AutomationState) -> Result<Option<ClipboardBackend>, CliError> {
    match &state.mode {
        ControllerMode::SendClipboard => ClipboardBackend::start(ClipboardMode::Watch).map(Some),
        ControllerMode::ReceiveClipboard => {
            ClipboardBackend::start(ClipboardMode::ApplyOnly).map(Some)
        }
        ControllerMode::SendPath { .. } | ControllerMode::ReceivePaths { .. } => Ok(None),
    }
}

async fn handle_clipboard_event(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    clipboard: Option<&ClipboardBackend>,
    log: &mut LogSink,
    event: ClipboardEvent,
) -> Result<(), CliError> {
    match event {
        ClipboardEvent::LocalTextChanged { text } => {
            if !state.is_send_clipboard_mode() {
                return Ok(());
            }
            let text_summary = clipboard_text_summary(&text);
            state.latest_clipboard_text = Some(text.clone());
            let targets = state.accepted_clipboard_channels();
            if targets.is_empty() {
                log.info(&format!(
                    "local clipboard changed ({text_summary}); waiting for an accepted peer"
                ));
                return Ok(());
            }
            for (channel_id, member_id) in targets {
                send_clipboard_payload(state, stdin, log, &channel_id, &member_id, &text).await?;
            }
        }
        ClipboardEvent::Warning { message } => {
            log.info(&format!("clipboard warning: {message}"));
            if clipboard.is_none() && state.allows_incoming_clipboard() {
                return Err(CliError::runtime(
                    "receive clipboard mode lost access to the local clipboard",
                ));
            }
        }
    }
    Ok(())
}

async fn handle_machine_event(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    clipboard: Option<&ClipboardBackend>,
    log: &mut LogSink,
    event: MachineEvent,
) -> Result<(), CliError> {
    match event {
        MachineEvent::RoomWelcome {
            room_id,
            self_member,
            host_member,
            ..
        } => {
            state.self_member = Some(self_member.clone());
            state.host_member = Some(host_member.clone());
            log.info(&format!(
                "joined room {} as {} (host {})",
                room_id.as_str(),
                self_member.as_str(),
                host_member.as_str()
            ));
        }
        MachineEvent::MemberSnapshot { members } => {
            state.members = members
                .into_iter()
                .map(|member| (member.member_id, member.name))
                .collect();
            log.info(&format!("members: {}", member_roster_line(state)));
            maybe_prepare_known_members(state, stdin, log).await?;
            maybe_finish_one_shot_send(state, stdin, log).await?;
        }
        MachineEvent::MemberJoined { member } => {
            let joined_id = member.member_id.clone();
            state.members.insert(member.member_id, member.name);
            log.info(&format!(
                "member joined: {}",
                state.display_member(&joined_id)
            ));
            maybe_prepare_member(state, stdin, log, &joined_id).await?;
            state.close_one_shot_targets_if_started();
            maybe_finish_one_shot_send(state, stdin, log).await?;
        }
        MachineEvent::MemberLeft { member_id, reason } => {
            let name = state.display_member(&member_id);
            state.members.remove(&member_id);
            let affected_channels = state
                .channels
                .iter()
                .filter_map(|(channel_id, channel)| match &channel.direction {
                    ChannelDirection::Outgoing { member_id: peer_id }
                    | ChannelDirection::Incoming {
                        member_id: peer_id, ..
                    } if peer_id == &member_id => Some(channel_id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            for channel_id in affected_channels {
                state.channels.remove(&channel_id);
            }
            state.pending_offers.remove(&member_id);
            let suffix = reason
                .as_deref()
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            log.info(&format!("member left: {name}{suffix}"));
            maybe_finish_one_shot_send(state, stdin, log).await?;
        }
        MachineEvent::RoomClosed { reason } => {
            if !(state.intentional_leave || (state.is_one_shot_send() && stdin.is_none())) {
                log.info(&format!("room closed: {reason}"));
            }
            stdin.take();
        }
        MachineEvent::ChannelOpened {
            channel_id,
            kind,
            from,
            from_name,
            name,
            size,
            transfer,
            ..
        } => {
            let Some(self_member) = state.local_member_id().cloned() else {
                return Ok(());
            };
            if from == self_member {
                return Ok(());
            }
            match kind {
                ChannelKind::File => {
                    let item_kind = transfer
                        .as_ref()
                        .map(|offer| offer.item_kind.clone())
                        .unwrap_or(TransferItemKind::File);
                    let item_name = name.unwrap_or_else(|| channel_id.as_str().to_string());
                    let size_suffix = size
                        .map(format_bytes)
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default();
                    let auto_accept = state.should_auto_accept_transfer(&item_kind);
                    let rejection_reason = state.transfer_rejection_reason(&item_kind);
                    log.info(&format!(
                        "incoming {} {} from {}{}",
                        transfer_item_kind_label(&item_kind),
                        item_name,
                        from_name,
                        size_suffix
                    ));
                    state.channels.insert(
                        channel_id.clone(),
                        ChannelState {
                            item_kind: AutomationItemKind::Transfer(item_kind.clone()),
                            name: item_name.clone(),
                            direction: ChannelDirection::Incoming {
                                member_id: from.clone(),
                                auto_accept,
                            },
                            progress: None,
                            accepted: false,
                        },
                    );
                    if auto_accept {
                        log.info(&format!(
                            "auto-accepting {} {} from {}",
                            transfer_item_kind_label(&item_kind),
                            item_name,
                            from_name
                        ));
                        send_machine_command(stdin, &MachineCommand::AcceptChannel { channel_id })
                            .await?;
                    } else {
                        log.info(&format!(
                            "rejecting {} {} from {} because this session is not in file receive mode",
                            transfer_item_kind_label(&item_kind),
                            item_name,
                            from_name
                        ));
                        send_machine_command(
                            stdin,
                            &MachineCommand::RejectChannel {
                                channel_id,
                                reason: Some(rejection_reason.into()),
                            },
                        )
                        .await?;
                    }
                }
                ChannelKind::Clipboard => {
                    let channel_name = name.unwrap_or_else(|| "clipboard".into());
                    let auto_accept = state.allows_incoming_clipboard();
                    log.info(&format!("incoming clipboard channel from {}", from_name));
                    state.channels.insert(
                        channel_id.clone(),
                        ChannelState {
                            item_kind: AutomationItemKind::Clipboard,
                            name: channel_name.clone(),
                            direction: ChannelDirection::Incoming {
                                member_id: from.clone(),
                                auto_accept,
                            },
                            progress: None,
                            accepted: false,
                        },
                    );
                    if auto_accept {
                        log.info(&format!(
                            "auto-accepting clipboard channel from {}",
                            from_name
                        ));
                        send_machine_command(stdin, &MachineCommand::AcceptChannel { channel_id })
                            .await?;
                    } else {
                        log.info(&format!(
                            "rejecting clipboard channel from {} because this session is not in clipboard receive mode",
                            from_name
                        ));
                        send_machine_command(
                            stdin,
                            &MachineCommand::RejectChannel {
                                channel_id,
                                reason: Some("peer is not in clipboard receive mode".into()),
                            },
                        )
                        .await?;
                    }
                }
                ChannelKind::Machine | ChannelKind::Pipe => {}
            }
        }
        MachineEvent::ChannelAccepted {
            channel_id,
            member_id,
            member_name,
        } => {
            let mut accepted_clipboard_target = None;
            if let Some(channel) = state.channels.get_mut(&channel_id) {
                channel.accepted = true;
                match &channel.direction {
                    ChannelDirection::Outgoing { .. } => log.info(&format!(
                        "{} accepted {} {}",
                        member_name,
                        automation_item_label(&channel.item_kind),
                        channel.name
                    )),
                    ChannelDirection::Incoming { auto_accept, .. } => {
                        if *auto_accept {
                            log.info(&format!(
                                "accepted {} {}",
                                automation_item_label(&channel.item_kind),
                                channel.name
                            ));
                        } else {
                            log.info(&format!(
                                "{} accepted {} {}",
                                member_name,
                                automation_item_label(&channel.item_kind),
                                channel.name
                            ));
                        }
                    }
                }
                if matches!(channel.item_kind, AutomationItemKind::Clipboard) {
                    if let ChannelDirection::Outgoing { member_id } = &channel.direction {
                        accepted_clipboard_target = Some(member_id.clone());
                    }
                }
            }
            state.clear_member_channel(&member_id, &channel_id);
            if let Some(target_member_id) = accepted_clipboard_target {
                if let Some(text) = state.latest_clipboard_text.clone() {
                    send_clipboard_payload(
                        state,
                        stdin,
                        log,
                        &channel_id,
                        &target_member_id,
                        &text,
                    )
                    .await?;
                }
            }
        }
        MachineEvent::ChannelTransferReady {
            channel_id,
            size,
            transfer,
        }
        | MachineEvent::ChannelTransferFinalized {
            channel_id,
            size,
            transfer,
        } => {
            if let Some(channel) = state.channels.get_mut(&channel_id) {
                channel.item_kind = AutomationItemKind::Transfer(transfer.item_kind.clone());
                if channel.name.is_empty() {
                    channel.name = channel_id.as_str().to_string();
                }
                if let Some(size) = size {
                    log.info(&format!(
                        "{} {} is ready ({})",
                        match channel.direction {
                            ChannelDirection::Outgoing { .. } => "outgoing",
                            ChannelDirection::Incoming { .. } => "incoming",
                        },
                        channel.name,
                        format_bytes(size)
                    ));
                }
            }
        }
        MachineEvent::ChannelRejected {
            channel_id,
            member_id,
            member_name,
            reason,
        } => {
            if let Some(channel) = state.channels.remove(&channel_id) {
                log.info(&format!(
                    "{} rejected {} {}{}",
                    member_name,
                    automation_item_label(&channel.item_kind),
                    channel.name,
                    reason
                        .as_deref()
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default()
                ));
            }
            state.clear_member_channel(&member_id, &channel_id);
            maybe_finish_one_shot_send(state, stdin, log).await?;
        }
        MachineEvent::ChannelClosed {
            channel_id,
            member_id,
            member_name,
            reason,
        } => {
            if let Some(channel) = state.channels.remove(&channel_id) {
                log.info(&format!(
                    "{} closed {} {}{}",
                    member_name,
                    automation_item_label(&channel.item_kind),
                    channel.name,
                    reason
                        .as_deref()
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default()
                ));
            }
            state.clear_member_channel(&member_id, &channel_id);
            maybe_finish_one_shot_send(state, stdin, log).await?;
        }
        MachineEvent::ChannelPathReceived {
            channel_id,
            path,
            size,
        } => {
            if let Some(channel) = state.channels.remove(&channel_id) {
                let detail = transfer_completion_detail(size, channel.progress.as_ref());
                match channel.direction {
                    ChannelDirection::Incoming { .. } => {
                        log.info(&format!(
                            "saved {} {} to {} ({})",
                            automation_item_label(&channel.item_kind),
                            channel.name,
                            display_path(&path),
                            detail
                        ));
                        if state.should_write_receive_to_stdout() {
                            write_received_file_to_stdout(log, &channel, &path)?;
                            state.stdout_receive_completed = true;
                            state.intentional_leave = true;
                            send_machine_command(stdin, &MachineCommand::LeaveRoom).await?;
                            log.info("left room");
                            stdin.take();
                        }
                    }
                    ChannelDirection::Outgoing { member_id } => log.info(&format!(
                        "completed {} {} to {} ({})",
                        automation_item_label(&channel.item_kind),
                        channel.name,
                        state.display_member(&member_id),
                        detail
                    )),
                }
                maybe_finish_one_shot_send(state, stdin, log).await?;
            }
        }
        MachineEvent::TransferProgress {
            channel_id,
            item_kind,
            name,
            phase,
            bytes_complete,
            bytes_total,
        } => {
            let tracked = state
                .record_progress(&channel_id, &phase, bytes_complete, bytes_total)
                .map(|progress| {
                    (
                        progress.phase.clone(),
                        progress.started_at.elapsed(),
                        progress.bytes_complete,
                        progress.bytes_total,
                    )
                });
            if let Some((effective_phase, elapsed, effective_complete, effective_total)) = tracked {
                if let Some(channel) = state.channels.get(&channel_id) {
                    let progress_text =
                        format_progress_with_speed(effective_complete, effective_total, elapsed)
                            .unwrap_or_else(|| format_bytes(effective_complete));
                    match &channel.direction {
                        ChannelDirection::Outgoing { member_id } => log.status(&format!(
                            "{} {} {} to {}: {}",
                            progress_verb(&effective_phase, true),
                            transfer_item_kind_label(&item_kind),
                            name,
                            state.display_member(member_id),
                            progress_text
                        )),
                        ChannelDirection::Incoming { member_id, .. } => log.status(&format!(
                            "{} {} {} from {}: {}",
                            progress_verb(&effective_phase, false),
                            transfer_item_kind_label(&item_kind),
                            name,
                            state.display_member(member_id),
                            progress_text
                        )),
                    }
                    if matches!(effective_phase, TransferPhase::Downloading)
                        && matches!(channel.direction, ChannelDirection::Outgoing { .. })
                        && effective_total.is_some_and(|total| effective_complete >= total)
                    {
                        let finished = state.channels.remove(&channel_id);
                        if let Some(ChannelState {
                            direction: ChannelDirection::Outgoing { member_id },
                            item_kind,
                            name,
                            progress,
                            ..
                        }) = finished
                        {
                            log.info(&format!(
                                "completed {} {} to {} ({})",
                                automation_item_label(&item_kind),
                                name,
                                state.display_member(&member_id),
                                transfer_completion_detail(
                                    effective_total.unwrap_or(effective_complete),
                                    progress.as_ref()
                                )
                            ));
                            maybe_finish_one_shot_send(state, stdin, log).await?;
                        }
                    }
                }
            }
        }
        MachineEvent::Error {
            code,
            message,
            channel_id,
        } => {
            if let Some(channel_id) = channel_id.as_ref() {
                if let Some(channel) = state.channels.remove(channel_id) {
                    log.info(&format!(
                        "error for {} {}: {} ({})",
                        automation_item_label(&channel.item_kind),
                        channel.name,
                        message,
                        code
                    ));
                } else {
                    log.info(&format!("backend error {code}: {message}"));
                }
                maybe_finish_one_shot_send(state, stdin, log).await?;
            } else {
                log.info(&format!("backend error {code}: {message}"));
            }
        }
        MachineEvent::ChannelData {
            channel_id,
            from,
            from_name,
            body,
        } => {
            let Some(channel) = state.channels.get(&channel_id) else {
                return Ok(());
            };
            if !matches!(channel.item_kind, AutomationItemKind::Clipboard) {
                return Ok(());
            }
            if !matches!(channel.direction, ChannelDirection::Incoming { .. }) {
                return Ok(());
            }
            let Some(clipboard) = clipboard else {
                return Err(CliError::runtime(
                    "clipboard backend is not available in clipboard receive mode",
                ));
            };
            let summary = clipboard_text_summary(&body);
            log.info(&format!(
                "received clipboard update from {} ({summary}); applying locally",
                state.display_member(&from)
            ));
            clipboard.set_text(body)?;
            log.info(&format!("applied clipboard update from {}", from_name));
        }
        MachineEvent::Chat { .. } => {}
    }
    Ok(())
}

async fn maybe_prepare_known_members(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    log: &mut LogSink,
) -> Result<(), CliError> {
    let member_ids = state.members.keys().cloned().collect::<Vec<_>>();
    for member_id in member_ids {
        maybe_prepare_member(state, stdin, log, &member_id).await?;
    }
    state.close_one_shot_targets_if_started();
    Ok(())
}

async fn maybe_prepare_member(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    log: &mut LogSink,
    member_id: &MemberId,
) -> Result<(), CliError> {
    if state
        .local_member_id()
        .is_some_and(|self_member| self_member == member_id)
    {
        return Ok(());
    }
    match &state.mode {
        ControllerMode::SendPath { source } => {
            let source = source.clone();
            if state.sent_members.contains(member_id)
                || state.pending_offers.contains_key(member_id)
            {
                return Ok(());
            }

            let channel_id = state.next_channel_id();
            state.sent_members.insert(member_id.clone());
            state
                .pending_offers
                .insert(member_id.clone(), channel_id.clone());
            state.channels.insert(
                channel_id.clone(),
                ChannelState {
                    item_kind: AutomationItemKind::Transfer(source.item_kind.clone()),
                    name: source.display_name.clone(),
                    direction: ChannelDirection::Outgoing {
                        member_id: member_id.clone(),
                    },
                    progress: None,
                    accepted: false,
                },
            );
            log.info(&format!(
                "offering {} {} to {}",
                transfer_item_kind_label(&source.item_kind),
                source.display_name,
                state.display_member(member_id)
            ));
            let command = MachineCommand::SendPath {
                channel_id,
                to: Route::Member {
                    member_id: member_id.clone(),
                },
                path: source.raw_path.clone(),
                name: Some(source.display_name.clone()),
                mime: None,
            };
            send_machine_command(stdin, &command).await?;
        }
        ControllerMode::SendClipboard => {
            if state.has_outgoing_clipboard_channel(member_id) {
                return Ok(());
            }

            let channel_id = state.next_channel_id();
            state.channels.insert(
                channel_id.clone(),
                ChannelState {
                    item_kind: AutomationItemKind::Clipboard,
                    name: "clipboard".into(),
                    direction: ChannelDirection::Outgoing {
                        member_id: member_id.clone(),
                    },
                    progress: None,
                    accepted: false,
                },
            );
            log.info(&format!(
                "opening clipboard channel to {}",
                state.display_member(member_id)
            ));
            send_machine_command(
                stdin,
                &MachineCommand::OpenChannel {
                    channel_id,
                    kind: ChannelKind::Clipboard,
                    to: Route::Member {
                        member_id: member_id.clone(),
                    },
                    name: Some("clipboard".into()),
                    size: None,
                    mime: Some("text/plain".into()),
                    transfer: None,
                },
            )
            .await?;
        }
        ControllerMode::ReceivePaths { .. } | ControllerMode::ReceiveClipboard => {}
    }
    Ok(())
}

async fn send_machine_command(
    stdin: &mut Option<tokio::process::ChildStdin>,
    command: &MachineCommand,
) -> Result<(), CliError> {
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| CliError::runtime("automation backend stdin is closed"))?;
    let line =
        serde_json::to_string(command).map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .write_all(line.as_bytes())
        .await
        .context("failed to write automation machine command")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate automation machine command")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .flush()
        .await
        .context("failed to flush automation machine command")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    Ok(())
}

async fn maybe_finish_one_shot_send(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    log: &mut LogSink,
) -> Result<(), CliError> {
    if stdin.is_none() || !state.should_finish_one_shot_send() {
        return Ok(());
    }
    log.info("stdin send complete; leaving room after receiver finalization grace period");
    state.intentional_leave = true;
    tokio::time::sleep(Duration::from_millis(250)).await;
    send_machine_command(stdin, &MachineCommand::LeaveRoom).await?;
    log.info("left room");
    stdin.take();
    Ok(())
}

fn write_received_file_to_stdout(
    log: &mut LogSink,
    channel: &ChannelState,
    path: &str,
) -> Result<(), CliError> {
    if !matches!(
        channel.item_kind,
        AutomationItemKind::Transfer(TransferItemKind::File)
    ) {
        return Err(CliError::usage(
            "--output - only supports received file transfers",
        ));
    }
    log.finish_status();
    log.info(&format!("writing file {} to stdout", channel.name));
    let mut file = fs::File::open(path).map_err(|error| {
        CliError::local_io(format!("failed to open received file {path}: {error}"))
    })?;
    let mut stdout = io::stdout().lock();
    io::copy(&mut file, &mut stdout).map_err(|error| {
        CliError::local_io(format!("failed to write received file to stdout: {error}"))
    })?;
    stdout
        .flush()
        .map_err(|error| CliError::local_io(format!("failed to flush stdout: {error}")))?;
    Ok(())
}

fn resolve_send_source(
    path: &str,
    display_name: Option<String>,
) -> Result<(SendSource, Option<PathBuf>), CliError> {
    if path == "-" {
        let display_name = display_name
            .ok_or_else(|| CliError::usage("--send - reads from stdin and requires --as <name>"))?;
        let spool_path = spool_stdin_to_temp_file(&display_name)?;
        return Ok((
            SendSource {
                raw_path: spool_path.display().to_string(),
                display_name,
                item_kind: TransferItemKind::File,
                kind: SendSourceKind::Stdin,
            },
            Some(spool_path),
        ));
    }

    let expanded_path = expand_user_path(path);
    let inferred_display_name = expanded_path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| expanded_path.display().to_string());
    let metadata = std::fs::metadata(&expanded_path).map_err(|error| {
        CliError::usage(format!(
            "failed to read {}: {error}",
            expanded_path.display()
        ))
    })?;
    let item_kind = if metadata.is_file() {
        TransferItemKind::File
    } else if metadata.is_dir() {
        TransferItemKind::Folder
    } else {
        return Err(CliError::usage(format!(
            "path {} is not a file or directory",
            expanded_path.display()
        )));
    };
    Ok((
        SendSource {
            raw_path: path.to_string(),
            display_name: display_name.unwrap_or(inferred_display_name),
            item_kind,
            kind: SendSourceKind::Path,
        },
        None,
    ))
}

fn spool_stdin_to_temp_file(display_name: &str) -> Result<PathBuf, CliError> {
    let temp_dir = std::env::temp_dir();
    let sanitized_name = Path::new(display_name)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("stdin.bin");
    for attempt in 0..100_u32 {
        let path = temp_dir.join(format!(
            "skyffla-stdin-{}-{attempt}-{sanitized_name}",
            std::process::id()
        ));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(CliError::local_io(format!(
                    "failed to create stdin spool file {}: {error}",
                    path.display()
                )));
            }
        };
        io::copy(&mut io::stdin().lock(), &mut file).map_err(|error| {
            CliError::local_io(format!(
                "failed to read stdin into {}: {error}",
                path.display()
            ))
        })?;
        file.flush().map_err(|error| {
            CliError::local_io(format!(
                "failed to flush stdin spool file {}: {error}",
                path.display()
            ))
        })?;
        return Ok(path);
    }
    Err(CliError::local_io(
        "failed to create a unique stdin spool file",
    ))
}

fn expand_user_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(candidate)
    }
}

fn member_roster_line(state: &AutomationState) -> String {
    if state.members.is_empty() {
        return "(waiting)".to_string();
    }
    state
        .members
        .keys()
        .map(|member_id| {
            let mut display = state.display_member(member_id);
            if state
                .local_member_id()
                .is_some_and(|self_member| self_member == member_id)
            {
                display.push('*');
            } else if state.host_member.as_ref() == Some(member_id) {
                display.push('^');
            }
            display
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn progress_verb(phase: &TransferPhase, outgoing: bool) -> &'static str {
    match phase {
        TransferPhase::Preparing => "preparing",
        TransferPhase::Downloading => {
            if outgoing {
                "sending"
            } else {
                "receiving"
            }
        }
        TransferPhase::Exporting => "saving",
    }
}

fn transfer_item_kind_label(kind: &TransferItemKind) -> &'static str {
    match kind {
        TransferItemKind::File => "file",
        TransferItemKind::Folder => "folder",
    }
}

fn automation_item_label(kind: &AutomationItemKind) -> &'static str {
    match kind {
        AutomationItemKind::Transfer(kind) => transfer_item_kind_label(kind),
        AutomationItemKind::Clipboard => "clipboard",
    }
}

fn clipboard_text_summary(text: &str) -> String {
    let chars = text.chars().count();
    if chars == 1 {
        "1 char".into()
    } else {
        format!("{chars} chars")
    }
}

async fn send_clipboard_payload(
    state: &AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    log: &mut LogSink,
    channel_id: &ChannelId,
    member_id: &MemberId,
    text: &str,
) -> Result<(), CliError> {
    send_machine_command(
        stdin,
        &MachineCommand::SendChannelData {
            channel_id: channel_id.clone(),
            body: text.to_string(),
        },
    )
    .await?;
    log.info(&format!(
        "sent clipboard update to {} ({})",
        state.display_member(member_id),
        clipboard_text_summary(text)
    ));
    Ok(())
}

fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn format_bytes(size: u64) -> String {
    if size < 1024 {
        return format!("{size}B");
    }
    if size < 1024 * 1024 {
        return format!("{:.1}KiB", size as f64 / 1024.0);
    }
    format!("{:.1}MiB", size as f64 / (1024.0 * 1024.0))
}

fn format_progress(bytes_complete: u64, bytes_total: Option<u64>) -> Option<String> {
    bytes_total.map(|bytes_total| {
        if bytes_total == 0 {
            return "0%".to_string();
        }
        let bytes_complete = bytes_complete.min(bytes_total);
        let percent = (bytes_complete.saturating_mul(100) / bytes_total).min(100);
        format!(
            "{} / {} ({}%)",
            format_bytes(bytes_complete),
            format_bytes(bytes_total),
            percent
        )
    })
}

fn format_progress_with_speed(
    bytes_complete: u64,
    bytes_total: Option<u64>,
    elapsed: Duration,
) -> Option<String> {
    let mut progress = format_progress(bytes_complete, bytes_total)?;
    if let Some(rate) = format_rate(bytes_complete, elapsed) {
        progress.push(' ');
        progress.push_str(&rate);
    }
    Some(progress)
}

fn format_rate(bytes: u64, elapsed: Duration) -> Option<String> {
    if elapsed < Duration::from_secs(1) {
        return None;
    }
    let bytes_per_second = bytes as f64 / elapsed.as_secs_f64();
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return None;
    }
    Some(format!(
        "{}/s",
        format_bytes(bytes_per_second.round() as u64)
    ))
}

fn transfer_completion_detail(size: u64, progress: Option<&ProgressState>) -> String {
    match progress
        .and_then(|progress| format_duration_and_rate(size, progress.started_at.elapsed()))
    {
        Some(detail) => format!("{} {detail}", format_bytes(size)),
        None => format_bytes(size),
    }
}

fn format_duration_and_rate(bytes: u64, elapsed: Duration) -> Option<String> {
    let elapsed = if elapsed >= Duration::from_secs(1) {
        elapsed
    } else {
        return None;
    };
    let rate = format_rate(bytes, elapsed)?;
    Some(format!("in {} at {}", format_duration(elapsed), rate))
}

fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m {remaining_seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn display_path(path: &str) -> String {
    if let Ok(current_dir) = std::env::current_dir() {
        if let Ok(relative) = Path::new(path).strip_prefix(&current_dir) {
            let rendered = relative.display().to_string();
            if !rendered.is_empty() {
                return rendered;
            }
        }
    }
    path.strip_prefix("./").unwrap_or(path).to_string()
}

fn backend_status_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event = value.get("event")?.as_str()?;
    match event {
        "waiting" => Some(format!(
            "waiting for room {} via {}",
            value.get("room_id")?.as_str()?,
            if value.get("discovery").is_some() {
                value.get("discovery")?.as_str()?
            } else {
                value.get("server")?.as_str()?
            }
        )),
        "connected" => Some(format!(
            "connected to {}",
            value.get("peer_name")?.as_str()?
        )),
        "connection_status" => Some(format!(
            "{} connection{}",
            value.get("mode")?.as_str()?,
            value
                .get("remote_addr")
                .and_then(Value::as_str)
                .map(|value| format!(" (remote {value})"))
                .unwrap_or_default()
        )),
        "room_link_connected" => Some(format!(
            "direct room link ready: {} ({})",
            value.get("member_name")?.as_str()?,
            value.get("member_id")?.as_str()?
        )),
        "error" => Some(format!(
            "backend error {}: {}",
            value.get("code")?.as_str()?,
            value.get("message")?.as_str()?
        )),
        _ => None,
    }
}

fn drain_backend_status_lines(
    stderr_rx: &mut mpsc::UnboundedReceiver<String>,
    last_backend_status: &mut Option<String>,
) {
    while let Ok(line) = stderr_rx.try_recv() {
        if let Some(message) = backend_status_line(&line) {
            *last_backend_status = Some(message);
        } else if serde_json::from_str::<Value>(&line).is_ok() {
            continue;
        } else if !line.trim().is_empty() {
            *last_backend_status = Some(line);
        }
    }
}

fn backend_exit_error(
    status: std::process::ExitStatus,
    last_backend_status: Option<String>,
) -> CliError {
    if let Some(message) = last_backend_status {
        return CliError::runtime(message);
    }
    if status.success() {
        CliError::runtime("automation backend exited unexpectedly")
    } else {
        CliError::runtime(format!("automation backend exited with status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use super::{
        progress_phase_rank, transfer_completion_detail, AutomationItemKind, AutomationState,
        ChannelDirection, ChannelState, ControllerMode, ProgressState, SendSource, SendSourceKind,
    };
    use skyffla_protocol::room::{ChannelId, MemberId, TransferItemKind, TransferPhase};
    use std::time::Instant;

    #[test]
    fn progress_phase_rank_is_monotonic_for_status_updates() {
        assert!(
            progress_phase_rank(&TransferPhase::Preparing)
                < progress_phase_rank(&TransferPhase::Downloading)
        );
        assert!(
            progress_phase_rank(&TransferPhase::Downloading)
                < progress_phase_rank(&TransferPhase::Exporting)
        );
    }

    #[test]
    fn tracked_progress_ignores_lower_phase_byte_regressions() {
        let channel_id = ChannelId::new("auto-1").expect("valid channel id");
        let member_id = MemberId::new("m2").expect("valid member id");
        let mut state = AutomationState {
            room_id: "room".into(),
            mode: ControllerMode::SendPath {
                source: SendSource {
                    raw_path: "report.txt".into(),
                    display_name: "report.txt".into(),
                    item_kind: TransferItemKind::File,
                    kind: SendSourceKind::Path,
                },
            },
            next_channel_index: 2,
            self_member: None,
            host_member: None,
            members: BTreeMap::new(),
            sent_members: BTreeSet::new(),
            pending_offers: BTreeMap::new(),
            channels: BTreeMap::from([(
                channel_id.clone(),
                ChannelState {
                    item_kind: AutomationItemKind::Transfer(TransferItemKind::File),
                    name: "report.txt".into(),
                    direction: ChannelDirection::Outgoing { member_id },
                    progress: Some(ProgressState {
                        phase: TransferPhase::Preparing,
                        started_at: Instant::now(),
                        bytes_complete: 40,
                        bytes_total: Some(100),
                    }),
                    accepted: false,
                },
            )]),
            latest_clipboard_text: None,
            stdin_spool_path: None,
            one_shot_targets_closed: false,
            intentional_leave: false,
            stdout_receive_completed: false,
        };

        let sending = state
            .record_progress(&channel_id, &TransferPhase::Downloading, 80, Some(100))
            .expect("progress should exist");
        assert_eq!(sending.phase, TransferPhase::Downloading);
        assert_eq!(sending.bytes_complete, 80);
        assert_eq!(sending.bytes_total, Some(100));

        let stale_prepare = state
            .record_progress(&channel_id, &TransferPhase::Preparing, 60, Some(100))
            .expect("progress should still exist");
        assert_eq!(stale_prepare.phase, TransferPhase::Downloading);
        assert_eq!(stale_prepare.bytes_complete, 80);
        assert_eq!(stale_prepare.bytes_total, Some(100));
    }

    #[test]
    fn transfer_completion_detail_includes_speed() {
        let progress = ProgressState {
            phase: TransferPhase::Downloading,
            started_at: Instant::now() - Duration::from_secs(2),
            bytes_complete: 16,
            bytes_total: Some(16),
        };
        assert_eq!(
            transfer_completion_detail(16, Some(&progress)),
            "16B in 2s at 8B/s"
        );
    }
}
