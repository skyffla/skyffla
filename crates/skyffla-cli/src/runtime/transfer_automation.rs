use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::Value;
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, MemberId, Route, TransferItemKind,
    TransferPhase,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::cli_error::CliError;
use crate::config::{AutomationMode, Role, SessionConfig};

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
    let mut last_backend_status = None;

    log_line(&state.startup_line());

    loop {
        tokio::select! {
            maybe_event = backend.stdout_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                handle_machine_event(&mut state, &mut backend.stdin, event).await?;
            }
            maybe_status = backend.stderr_rx.recv() => {
                let Some(line) = maybe_status else {
                    continue;
                };
                if let Some(message) = status_line(&line) {
                    last_backend_status = Some(message.clone());
                    log_line(&message);
                } else if !line.trim().is_empty() {
                    last_backend_status = Some(line.clone());
                    log_line(&line);
                }
            }
            status = backend.child.wait() => {
                let status = status.map_err(|error| CliError::runtime(error.to_string()))?;
                drain_backend_status_lines(&mut backend.stderr_rx, &mut last_backend_status);
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
    if status.success() {
        Ok(())
    } else {
        Err(backend_exit_error(status, last_backend_status))
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
}

enum ControllerMode {
    Send { source: SendSource },
    Receive,
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
}

struct ChannelState {
    item_kind: TransferItemKind,
    name: String,
    direction: ChannelDirection,
    progress: Option<ProgressState>,
}

struct ProgressState {
    phase: TransferPhase,
    started_at: Instant,
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
        let mode = match automation {
            AutomationMode::Send { path } => ControllerMode::Send {
                source: resolve_send_source(&path)?,
            },
            AutomationMode::Receive => ControllerMode::Receive,
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
        })
    }

    fn startup_line(&self) -> String {
        match &self.mode {
            ControllerMode::Send { source } => format!(
                "send mode: staying online in room {} and offering {} {} to each member once",
                self.room_id,
                item_kind_label(source.item_kind.clone()),
                source.display_name
            ),
            ControllerMode::Receive => format!(
                "receive mode: staying online in room {} and auto-accepting incoming transfers",
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
    ) -> Option<&ProgressState> {
        let channel = self.channels.get_mut(channel_id)?;
        let replace = channel
            .progress
            .as_ref()
            .is_none_or(|progress| progress.phase != *phase);
        if replace {
            channel.progress = Some(ProgressState {
                phase: phase.clone(),
                started_at: Instant::now(),
            });
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
}

async fn handle_machine_event(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
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
            log_line(&format!(
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
            log_line(&format!("members: {}", member_roster_line(state)));
            maybe_offer_to_known_members(state, stdin).await?;
        }
        MachineEvent::MemberJoined { member } => {
            let joined_id = member.member_id.clone();
            state.members.insert(member.member_id, member.name);
            log_line(&format!(
                "member joined: {}",
                state.display_member(&joined_id)
            ));
            maybe_offer_to_member(state, stdin, &joined_id).await?;
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
            log_line(&format!("member left: {name}{suffix}"));
        }
        MachineEvent::RoomClosed { reason } => {
            log_line(&format!("room closed: {reason}"));
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
            if kind != ChannelKind::File {
                return Ok(());
            }
            let Some(self_member) = state.local_member_id().cloned() else {
                return Ok(());
            };
            if from == self_member {
                return Ok(());
            }
            let item_kind = transfer
                .as_ref()
                .map(|offer| offer.item_kind.clone())
                .unwrap_or(TransferItemKind::File);
            let item_name = name.unwrap_or_else(|| channel_id.as_str().to_string());
            let size_suffix = size
                .map(format_bytes)
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            match &state.mode {
                ControllerMode::Receive => {
                    log_line(&format!(
                        "incoming {} {} from {}{}",
                        item_kind_label(item_kind.clone()),
                        item_name,
                        from_name,
                        size_suffix
                    ));
                    state.channels.insert(
                        channel_id.clone(),
                        ChannelState {
                            item_kind: item_kind.clone(),
                            name: item_name.clone(),
                            direction: ChannelDirection::Incoming {
                                member_id: from.clone(),
                                auto_accept: true,
                            },
                            progress: None,
                        },
                    );
                    log_line(&format!(
                        "auto-accepting {} {} from {}",
                        item_kind_label(item_kind),
                        item_name,
                        from_name
                    ));
                    send_machine_command(stdin, &MachineCommand::AcceptChannel { channel_id })
                        .await?;
                }
                ControllerMode::Send { .. } => {
                    log_line(&format!(
                        "incoming {} {} from {}{}; rejecting because this session is in send mode",
                        item_kind_label(item_kind.clone()),
                        item_name,
                        from_name,
                        size_suffix
                    ));
                    state.channels.insert(
                        channel_id.clone(),
                        ChannelState {
                            item_kind,
                            name: item_name,
                            direction: ChannelDirection::Incoming {
                                member_id: from.clone(),
                                auto_accept: false,
                            },
                            progress: None,
                        },
                    );
                    send_machine_command(
                        stdin,
                        &MachineCommand::RejectChannel {
                            channel_id,
                            reason: Some("peer is in send mode".into()),
                        },
                    )
                    .await?;
                }
            }
        }
        MachineEvent::ChannelAccepted {
            channel_id,
            member_id,
            member_name,
        } => {
            if let Some(channel) = state.channels.get(&channel_id) {
                match &channel.direction {
                    ChannelDirection::Outgoing { .. } => log_line(&format!(
                        "{} accepted {} {}",
                        member_name,
                        item_kind_label(channel.item_kind.clone()),
                        channel.name
                    )),
                    ChannelDirection::Incoming { auto_accept, .. } => {
                        if *auto_accept {
                            log_line(&format!(
                                "accepted {} {}",
                                item_kind_label(channel.item_kind.clone()),
                                channel.name
                            ));
                        } else {
                            log_line(&format!(
                                "{} accepted {} {}",
                                member_name,
                                item_kind_label(channel.item_kind.clone()),
                                channel.name
                            ));
                        }
                    }
                }
            }
            state.clear_member_channel(&member_id, &channel_id);
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
                channel.item_kind = transfer.item_kind.clone();
                if channel.name.is_empty() {
                    channel.name = channel_id.as_str().to_string();
                }
                if let Some(size) = size {
                    log_line(&format!(
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
                log_line(&format!(
                    "{} rejected {} {}{}",
                    member_name,
                    item_kind_label(channel.item_kind),
                    channel.name,
                    reason
                        .as_deref()
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default()
                ));
            }
            state.clear_member_channel(&member_id, &channel_id);
        }
        MachineEvent::ChannelClosed {
            channel_id,
            member_id,
            member_name,
            reason,
        } => {
            if let Some(channel) = state.channels.remove(&channel_id) {
                log_line(&format!(
                    "{} closed {} {}{}",
                    member_name,
                    item_kind_label(channel.item_kind),
                    channel.name,
                    reason
                        .as_deref()
                        .map(|value| format!(" ({value})"))
                        .unwrap_or_default()
                ));
            }
            state.clear_member_channel(&member_id, &channel_id);
        }
        MachineEvent::ChannelPathReceived {
            channel_id,
            path,
            size,
        } => {
            if let Some(channel) = state.channels.remove(&channel_id) {
                match channel.direction {
                    ChannelDirection::Incoming { .. } => log_line(&format!(
                        "saved {} {} to {} ({})",
                        item_kind_label(channel.item_kind),
                        channel.name,
                        display_path(&path),
                        format_bytes(size)
                    )),
                    ChannelDirection::Outgoing { member_id } => log_line(&format!(
                        "completed {} {} to {} ({})",
                        item_kind_label(channel.item_kind),
                        channel.name,
                        state.display_member(&member_id),
                        format_bytes(size)
                    )),
                }
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
            let elapsed = state
                .record_progress(&channel_id, &phase)
                .map(|progress| progress.started_at.elapsed());
            if let Some(elapsed) = elapsed {
                if let Some(channel) = state.channels.get(&channel_id) {
                    let progress_text =
                        format_progress_with_speed(bytes_complete, bytes_total, elapsed)
                            .unwrap_or_else(|| format_bytes(bytes_complete));
                    match &channel.direction {
                        ChannelDirection::Outgoing { member_id } => log_line(&format!(
                            "{} {} {} to {}: {}",
                            progress_verb(&phase, true),
                            item_kind_label(item_kind.clone()),
                            name,
                            state.display_member(member_id),
                            progress_text
                        )),
                        ChannelDirection::Incoming { member_id, .. } => log_line(&format!(
                            "{} {} {} from {}: {}",
                            progress_verb(&phase, false),
                            item_kind_label(item_kind.clone()),
                            name,
                            state.display_member(member_id),
                            progress_text
                        )),
                    }
                    if matches!(phase, TransferPhase::Downloading)
                        && matches!(channel.direction, ChannelDirection::Outgoing { .. })
                        && bytes_total.is_some_and(|total| bytes_complete >= total)
                    {
                        let finished = state.channels.remove(&channel_id);
                        if let Some(ChannelState {
                            direction: ChannelDirection::Outgoing { member_id },
                            item_kind,
                            name,
                            ..
                        }) = finished
                        {
                            log_line(&format!(
                                "completed {} {} to {}",
                                item_kind_label(item_kind),
                                name,
                                state.display_member(&member_id)
                            ));
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
                    log_line(&format!(
                        "transfer error for {} {}: {} ({})",
                        item_kind_label(channel.item_kind),
                        channel.name,
                        message,
                        code
                    ));
                } else {
                    log_line(&format!("backend error {code}: {message}"));
                }
            } else {
                log_line(&format!("backend error {code}: {message}"));
            }
        }
        MachineEvent::Chat { .. } | MachineEvent::ChannelData { .. } => {}
    }
    Ok(())
}

async fn maybe_offer_to_known_members(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
) -> Result<(), CliError> {
    let member_ids = state.members.keys().cloned().collect::<Vec<_>>();
    for member_id in member_ids {
        maybe_offer_to_member(state, stdin, &member_id).await?;
    }
    Ok(())
}

async fn maybe_offer_to_member(
    state: &mut AutomationState,
    stdin: &mut Option<tokio::process::ChildStdin>,
    member_id: &MemberId,
) -> Result<(), CliError> {
    let ControllerMode::Send { source } = &state.mode else {
        return Ok(());
    };
    let source = source.clone();
    if state
        .local_member_id()
        .is_some_and(|self_member| self_member == member_id)
    {
        return Ok(());
    }
    if state.sent_members.contains(member_id) || state.pending_offers.contains_key(member_id) {
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
            item_kind: source.item_kind.clone(),
            name: source.display_name.clone(),
            direction: ChannelDirection::Outgoing {
                member_id: member_id.clone(),
            },
            progress: None,
        },
    );
    log_line(&format!(
        "offering {} {} to {}",
        item_kind_label(source.item_kind.clone()),
        source.display_name,
        state.display_member(member_id)
    ));
    let command = MachineCommand::SendPath {
        channel_id,
        to: Route::Member {
            member_id: member_id.clone(),
        },
        path: source.raw_path.clone(),
        name: None,
        mime: None,
    };
    send_machine_command(stdin, &command).await?;
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

fn resolve_send_source(path: &str) -> Result<SendSource, CliError> {
    let expanded_path = expand_user_path(path);
    let display_name = expanded_path
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
    Ok(SendSource {
        raw_path: path.to_string(),
        display_name,
        item_kind,
    })
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

fn item_kind_label(kind: TransferItemKind) -> &'static str {
    match kind {
        TransferItemKind::File => "file",
        TransferItemKind::Folder => "folder",
    }
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

fn log_line(message: &str) {
    println!("{message}");
}

fn status_line(line: &str) -> Option<String> {
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
        if let Some(message) = status_line(&line) {
            *last_backend_status = Some(message);
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
