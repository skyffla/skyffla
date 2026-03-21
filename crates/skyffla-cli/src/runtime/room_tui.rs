use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use serde_json::Value;
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, MemberId, Route, TransferItemKind,
    TransferPhase,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration as TokioDuration};

use crate::cli_error::CliError;
use crate::config::{Role, SessionConfig};
use crate::runtime::machine_command_line::parse_machine_command_line;
use crate::ui::{TerminalUiGuard, UiState};

pub(crate) async fn run_room_tui(role: Role, config: &SessionConfig) -> Result<(), CliError> {
    if scripted_mode() {
        return run_scripted_room_tui(role, config).await;
    }

    let _terminal =
        TerminalUiGuard::activate().map_err(|error| CliError::runtime(error.to_string()))?;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if input_tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let mut backend = spawn_machine_backend(role, config).await?;
    let mut ui = UiState::new(&config.stream_id, &config.peer_name, "room")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    let mut state = RoomTuiState::new(&config.peer_name, config.download_dir.clone());

    ui.system("room session ready; use /help for commands".to_string());
    ui.render();

    loop {
        tokio::select! {
            maybe_key = input_rx.recv() => {
                let Some(key) = maybe_key else { break; };
                if let Some(line) = ui.handle_key_event(key) {
                    match parse_room_tui_input(&line, &mut state) {
                        Ok(RoomTuiInput::Quit) => {
                            let _ = request_backend_leave(&mut backend).await;
                            let _ = shutdown_backend_child(&mut backend).await;
                            break;
                        }
                        Ok(RoomTuiInput::Help) => {
                            for line in room_help_lines() {
                                ui.system((*line).to_string());
                            }
                        }
                        Ok(RoomTuiInput::ShowMembers) => {
                            ui.system(state.member_roster_line());
                        }
                        Ok(RoomTuiInput::Send(command)) => {
                            send_machine_command(backend.stdin.as_mut(), &command).await?;
                            apply_room_lines(&mut ui, local_command_feedback_lines(&mut state, &command));
                        }
                        Err(error) => ui.system(error.to_string()),
                    }
                }
                ui.render();
            }
            maybe_event = backend.stdout_rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        let terminal_event = matches!(event, MachineEvent::RoomClosed { .. });
                        let follow_up = apply_room_event(&mut state, &mut ui, event);
                        if let Some(command) = follow_up {
                            send_machine_command(backend.stdin.as_mut(), &command).await?;
                            apply_room_lines(&mut ui, local_command_feedback_lines(&mut state, &command));
                        }
                        ui.render();
                        if terminal_event {
                            let _ = shutdown_backend_child(&mut backend).await;
                            break;
                        }
                    }
                    None => break,
                }
            }
            maybe_status = backend.stderr_rx.recv() => {
                match maybe_status {
                    Some(line) => {
                        if let Some(message) = status_line(&line) {
                            ui.system(message);
                            ui.render();
                        }
                    }
                    None => {}
                }
            }
            status = backend.child.wait() => {
                let status = status.map_err(|error| CliError::runtime(error.to_string()))?;
                if !status.success() {
                    ui.system(format!("backend exited with status {status}"));
                    ui.render();
                }
                break;
            }
        }
    }

    Ok(())
}

async fn run_scripted_room_tui(role: Role, config: &SessionConfig) -> Result<(), CliError> {
    let mut backend = spawn_machine_backend(role, config).await?;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut state = RoomTuiState::new(&config.peer_name, config.download_dir.clone());

    loop {
        tokio::select! {
            line = input.next_line() => {
                match line {
                    Ok(Some(line)) => match parse_room_tui_input(&line, &mut state) {
                        Ok(RoomTuiInput::Quit) => {
                            let _ = request_backend_leave(&mut backend).await;
                            let _ = shutdown_backend_child(&mut backend).await;
                            break;
                        }
                        Ok(RoomTuiInput::Help) => {
                            for line in room_help_lines() {
                                emit_scripted_line(line).await?;
                            }
                        }
                        Ok(RoomTuiInput::ShowMembers) => {
                            emit_scripted_line(&state.member_roster_line()).await?;
                        }
                        Ok(RoomTuiInput::Send(command)) => {
                            send_machine_command(backend.stdin.as_mut(), &command).await?;
                            for line in stringify_room_lines(local_command_feedback_lines(&mut state, &command)) {
                                emit_scripted_line(&line).await?;
                            }
                        }
                        Err(error) => emit_scripted_line(&format!("error: {error}")).await?,
                    },
                    Ok(None) => {
                        let _ = backend.child.kill().await;
                        break;
                    }
                    Err(error) => return Err(CliError::runtime(error.to_string())),
                }
            }
            maybe_event = backend.stdout_rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        let terminal_event = matches!(event, MachineEvent::RoomClosed { .. });
                        let (lines, follow_up) = summarize_room_event(&mut state, event);
                        for line in lines {
                            emit_scripted_line(&line).await?;
                        }
                        if let Some(command) = follow_up {
                            send_machine_command(backend.stdin.as_mut(), &command).await?;
                            for line in stringify_room_lines(local_command_feedback_lines(&mut state, &command)) {
                                emit_scripted_line(&line).await?;
                            }
                        }
                        if terminal_event {
                            let _ = shutdown_backend_child(&mut backend).await;
                            break;
                        }
                    }
                    None => break,
                }
            }
            maybe_status = backend.stderr_rx.recv() => {
                if let Some(line) = maybe_status {
                    if let Some(message) = status_line(&line) {
                        emit_scripted_line(&message).await?;
                    }
                }
            }
            status = backend.child.wait() => {
                let status = status.map_err(|error| CliError::runtime(error.to_string()))?;
                if !status.success() {
                    emit_scripted_line(&format!("backend exited with status {status}")).await?;
                }
                break;
            }
        }
    }

    Ok(())
}

async fn emit_scripted_line(line: &str) -> Result<(), CliError> {
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(line.as_bytes())
        .await
        .context("failed to write scripted room tui output")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdout
        .write_all(b"\n")
        .await
        .context("failed to write scripted room tui newline")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdout
        .flush()
        .await
        .context("failed to flush scripted room tui output")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    Ok(())
}

struct RoomBackend {
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_rx: mpsc::UnboundedReceiver<MachineEvent>,
    stderr_rx: mpsc::UnboundedReceiver<String>,
}

enum RoomLine {
    System(String),
    Chat { speaker: String, text: String },
}

async fn spawn_machine_backend(
    role: Role,
    config: &SessionConfig,
) -> Result<RoomBackend, CliError> {
    let exe = std::env::current_exe().map_err(|error| CliError::local_io(error.to_string()))?;
    let mut command = Command::new(exe);
    command.arg(match role {
        Role::Host => "host",
        Role::Join => "join",
    });
    command.arg(&config.stream_id);
    command.arg("machine");
    command.arg("--server").arg(&config.rendezvous_server);
    command.arg("--download-dir").arg(&config.download_dir);
    command.arg("--name").arg(&config.peer_name);
    command.arg("--json");
    if config.local_mode {
        command.arg("--local");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .context("failed to spawn room machine backend")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    let stdin = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::runtime("machine backend stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| CliError::runtime("machine backend stderr missing"))?;
    let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
    let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
    spawn_machine_stdout_reader(stdout, stdout_tx);
    spawn_machine_stderr_reader(stderr, stderr_tx);

    Ok(RoomBackend {
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

async fn send_machine_command(
    stdin: Option<&mut tokio::process::ChildStdin>,
    command: &MachineCommand,
) -> Result<(), CliError> {
    let stdin = stdin.ok_or_else(|| CliError::runtime("room backend stdin is closed"))?;
    let line =
        serde_json::to_string(command).map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .write_all(line.as_bytes())
        .await
        .context("failed to write machine command")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate machine command line")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    stdin
        .flush()
        .await
        .context("failed to flush machine command")
        .map_err(|error| CliError::runtime(error.to_string()))?;
    Ok(())
}

async fn request_backend_leave(backend: &mut RoomBackend) -> Result<(), CliError> {
    send_machine_command(backend.stdin.as_mut(), &MachineCommand::LeaveRoom).await?;
    backend.stdin.take();
    Ok(())
}

async fn shutdown_backend_child(backend: &mut RoomBackend) -> Result<(), CliError> {
    backend.stdin.take();
    match timeout(TokioDuration::from_secs(1), backend.child.wait()).await {
        Ok(result) => {
            let _status = result
                .context("room backend failed while waiting for shutdown")
                .map_err(|error| CliError::runtime(error.to_string()))?;
        }
        Err(_) => {
            let _ = backend.child.kill().await;
            let _ = timeout(TokioDuration::from_secs(1), backend.child.wait()).await;
        }
    };
    Ok(())
}

#[derive(Debug)]
enum RoomTuiInput {
    Quit,
    Help,
    ShowMembers,
    Send(MachineCommand),
}

fn parse_room_tui_input(line: &str, state: &mut RoomTuiState) -> Result<RoomTuiInput, CliError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage("command must not be empty"));
    }
    if matches!(trimmed, "q" | "/quit") {
        return Ok(RoomTuiInput::Quit);
    }
    if trimmed == "/help" {
        return Ok(RoomTuiInput::Help);
    }
    if trimmed == "/members" {
        return Ok(RoomTuiInput::ShowMembers);
    }
    if let Some(rest) = trimmed.strip_prefix("/msg ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let member = parts
            .next()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CliError::usage("/msg requires <member_id> <text>"))?;
        let text = parts
            .next()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CliError::usage("/msg requires <member_id> <text>"))?;
        return Ok(RoomTuiInput::Send(MachineCommand::SendChat {
            to: Route::Member {
                member_id: state.resolve_member(member)?,
            },
            text: text.to_string(),
        }));
    }
    if let Some(rest) = trimmed.strip_prefix("/send ") {
        let tokens = split_shell_words(rest)?;
        if tokens.is_empty() {
            return Err(CliError::usage(
                "/send requires <path> or <all|member_id|name> <path>",
            ));
        }
        let (to, path) = if tokens.len() == 1 {
            (
                Route::Member {
                    member_id: state.default_send_target()?,
                },
                tokens[0].clone(),
            )
        } else {
            (parse_route(&tokens[0], state)?, tokens[1..].join(" "))
        };
        let command = MachineCommand::SendPath {
            channel_id: state.next_local_file_channel(),
            to,
            path,
            name: None,
            mime: None,
        };
        command
            .validate()
            .map_err(|error| CliError::usage(error.to_string()))?;
        return Ok(RoomTuiInput::Send(command));
    }
    if trimmed == "/accept" {
        let channel_id = state.default_pending_file_channel().ok_or_else(|| {
            CliError::usage("/accept requires a pending file or /accept <channel_id>")
        })?;
        return Ok(RoomTuiInput::Send(MachineCommand::AcceptChannel {
            channel_id,
        }));
    }
    if let Some(rest) = trimmed.strip_prefix("/accept ") {
        return Ok(RoomTuiInput::Send(MachineCommand::AcceptChannel {
            channel_id: ChannelId::new(rest.trim().to_string())
                .map_err(|error| CliError::usage(error.to_string()))?,
        }));
    }
    if let Some(rest) = trimmed.strip_prefix("/reject ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let channel_id = parts
            .next()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CliError::usage("/reject requires <channel_id> [reason]"))?;
        let reason = parts.next().map(|value| value.trim().to_string());
        return Ok(RoomTuiInput::Send(MachineCommand::RejectChannel {
            channel_id: ChannelId::new(channel_id.to_string())
                .map_err(|error| CliError::usage(error.to_string()))?,
            reason,
        }));
    }
    if trimmed == "/reject" {
        let channel_id = state.default_pending_file_channel().ok_or_else(|| {
            CliError::usage("/reject requires a pending file or /reject <channel_id> [reason]")
        })?;
        return Ok(RoomTuiInput::Send(MachineCommand::RejectChannel {
            channel_id,
            reason: None,
        }));
    }
    if trimmed == "/save" || trimmed.starts_with("/save ") {
        return Err(CliError::usage(
            "accepted transfers save automatically; /save is no longer needed",
        ));
    }
    if trimmed.starts_with("/file ")
        || trimmed.starts_with("/channel ")
        || trimmed.starts_with("/chat ")
    {
        return Ok(RoomTuiInput::Send(parse_machine_command_line(trimmed)?));
    }
    Ok(RoomTuiInput::Send(MachineCommand::SendChat {
        to: Route::All,
        text: trimmed.to_string(),
    }))
}

fn parse_route(value: &str, state: &RoomTuiState) -> Result<Route, CliError> {
    if value == "all" {
        return Ok(Route::All);
    }
    Ok(Route::Member {
        member_id: state.resolve_member(value)?,
    })
}

fn apply_room_lines(ui: &mut UiState, lines: Vec<RoomLine>) {
    for line in lines {
        match line {
            RoomLine::System(text) => ui.system(text),
            RoomLine::Chat { speaker, text } => ui.chat(&speaker, &text),
        }
    }
}

fn stringify_room_lines(lines: Vec<RoomLine>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| match line {
            RoomLine::System(text) => text,
            RoomLine::Chat { speaker, text } => format!("{speaker}: {text}"),
        })
        .collect()
}

fn local_command_feedback_lines(
    state: &mut RoomTuiState,
    command: &MachineCommand,
) -> Vec<RoomLine> {
    match command {
        MachineCommand::SendChat { to, text } => match to {
            Route::All => vec![RoomLine::Chat {
                speaker: "you".to_string(),
                text: text.clone(),
            }],
            Route::Member { member_id } => {
                let target = state.display_member(member_id);
                vec![RoomLine::Chat {
                    speaker: format!("you -> {target}"),
                    text: text.clone(),
                }]
            }
        },
        MachineCommand::SendFile {
            channel_id,
            to,
            path,
            ..
        }
        | MachineCommand::SendPath {
            channel_id,
            to,
            path,
            ..
        } => {
            let item_kind = path_item_kind(path);
            let name = Path::new(path)
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            state
                .file_channels
                .entry(channel_id.clone())
                .or_insert(FileChannelUiState {
                    item_kind: item_kind.clone(),
                    name: name.clone(),
                    size: None,
                    is_outgoing: true,
                    transfer_ready: item_kind != TransferItemKind::File,
                    accepted: false,
                    accepted_by: None,
                });
            let route = match to {
                Route::All => "all".to_string(),
                Route::Member { member_id } => state.display_member(member_id),
            };
            vec![RoomLine::System(format!(
                "preparing {} {} to send to {}",
                item_kind_label(item_kind),
                name,
                route
            ))]
        }
        MachineCommand::AcceptChannel { channel_id } => {
            let Some(file) = state.file_channels.get(channel_id) else {
                return vec![RoomLine::System(format!("accepting {}", channel_id.as_str()))];
            };
            let label = state
                .channel_summary(channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            if file.transfer_ready {
                vec![RoomLine::System(format!("accepting {label}"))]
            } else {
                vec![RoomLine::System(format!(
                    "accepted {label}; waiting for sender to finish preparing"
                ))]
            }
        }
        MachineCommand::RejectChannel { channel_id, .. } => {
            let label = state
                .channel_summary(channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            vec![RoomLine::System(format!("rejecting {label}"))]
        }
        _ => Vec::new(),
    }
}

fn split_shell_words(line: &str) -> Result<Vec<String>, CliError> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match quote {
            Some(active) => match ch {
                '\\' if active == '"' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| CliError::usage("unfinished escape in command"))?;
                    current.push(next);
                }
                value if value == active => quote = None,
                _ => current.push(ch),
            },
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| CliError::usage("unfinished escape in command"))?;
                    current.push(next);
                }
                value if value.is_whitespace() => {
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }
    if quote.is_some() {
        return Err(CliError::usage("unterminated quoted string in command"));
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

struct RoomTuiState {
    local_name: String,
    download_dir: PathBuf,
    next_file_channel_index: u64,
    self_member: Option<MemberId>,
    host_member: Option<MemberId>,
    members: BTreeMap<MemberId, String>,
    file_channels: BTreeMap<ChannelId, FileChannelUiState>,
    transfer_metrics: BTreeMap<ChannelId, TransferUiMetrics>,
    pending_incoming_files: Vec<ChannelId>,
    active_progress_channel: Option<ChannelId>,
}

impl RoomTuiState {
    fn new(local_name: &str, download_dir: PathBuf) -> Self {
        Self {
            local_name: local_name.to_string(),
            download_dir,
            next_file_channel_index: 1,
            self_member: None,
            host_member: None,
            members: BTreeMap::new(),
            file_channels: BTreeMap::new(),
            transfer_metrics: BTreeMap::new(),
            pending_incoming_files: Vec::new(),
            active_progress_channel: None,
        }
    }

    fn next_local_file_channel(&mut self) -> ChannelId {
        let id = ChannelId::new(format!("file-{}", self.next_file_channel_index))
            .expect("generated channel ids should be valid");
        self.next_file_channel_index += 1;
        id
    }

    fn member_roster_line(&self) -> String {
        if self.members.is_empty() {
            return "members: (waiting)".to_string();
        }
        let mut parts = Vec::new();
        for member_id in self.members.keys() {
            let marker = if self.self_member.as_ref() == Some(member_id) {
                "*"
            } else if self.host_member.as_ref() == Some(member_id) {
                "^"
            } else {
                ""
            };
            parts.push(format!("{}{}", self.display_member(member_id), marker));
        }
        format!("members: {}", parts.join(", "))
    }

    fn default_pending_file_channel(&self) -> Option<ChannelId> {
        self.pending_incoming_files.last().cloned()
    }

    fn channel_summary(&self, channel_id: &ChannelId) -> Option<String> {
        let file = self.file_channels.get(channel_id)?;
        let size_suffix = file
            .size
            .map(format_bytes)
            .map(|value| format!(" {value}"))
            .unwrap_or_default();
        Some(format!(
            "{} {}{}",
            item_kind_label(file.item_kind.clone()),
            file.name,
            size_suffix
        ))
    }

    fn display_member(&self, member_id: &MemberId) -> String {
        self.members
            .get(member_id)
            .map(|name| self.display_member_name(name, member_id))
            .unwrap_or_else(|| member_id.as_str().to_string())
    }

    fn display_member_name(&self, name: &str, member_id: &MemberId) -> String {
        if self
            .members
            .values()
            .filter(|candidate| candidate.as_str() == name)
            .take(2)
            .count()
            > 1
        {
            format!("{name} ({})", member_id.as_str())
        } else {
            name.to_string()
        }
    }

    fn display_member_name_with_fallback(&self, name: &str, member_id: &MemberId) -> String {
        if self.members.contains_key(member_id) {
            self.display_member(member_id)
        } else {
            self.display_member_name(name, member_id)
        }
    }

    fn resolve_member(&self, value: &str) -> Result<MemberId, CliError> {
        let mut matches = self
            .members
            .iter()
            .filter(|(member_id, name)| {
                self.self_member.as_ref() != Some(*member_id) && name == &value
            })
            .map(|(member_id, _)| member_id.clone())
            .collect::<Vec<_>>();
        let name_match = match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(CliError::usage(format!("unknown room member {value}"))),
            _ => Err(CliError::usage(format!("member name {value} is ambiguous"))),
        };

        name_match.or_else(|name_error| {
            let member_id = MemberId::new(value.to_string())
                .map_err(|error| CliError::usage(error.to_string()))?;
            if self.self_member.as_ref() == Some(&member_id) {
                return Err(name_error);
            }
            if self.members.contains_key(&member_id) {
                Ok(member_id)
            } else {
                Err(name_error)
            }
        })
    }

    fn default_send_target(&self) -> Result<MemberId, CliError> {
        let others = self
            .members
            .keys()
            .filter(|member_id| self.self_member.as_ref() != Some(*member_id))
            .cloned()
            .collect::<Vec<_>>();
        match others.as_slice() {
            [member_id] => Ok(member_id.clone()),
            [] => Err(CliError::usage("no other room member is available yet")),
            _ => Err(CliError::usage(
                "/send requires a target when more than one other room member is present",
            )),
        }
    }

    fn set_active_progress_channel(&mut self, channel_id: ChannelId) {
        self.active_progress_channel = Some(channel_id);
    }

    fn clear_active_progress_if_matches(&mut self, channel_id: &ChannelId) {
        if self.active_progress_channel.as_ref() == Some(channel_id) {
            self.active_progress_channel = None;
        }
    }

    fn record_transfer_progress(
        &mut self,
        channel_id: &ChannelId,
        phase: &TransferPhase,
        bytes_complete: u64,
        bytes_total: Option<u64>,
    ) -> &TransferUiMetrics {
        let entry = self
            .transfer_metrics
            .entry(channel_id.clone())
            .or_insert_with(|| TransferUiMetrics::new(phase.clone(), bytes_complete, bytes_total));
        if entry.phase != *phase {
            *entry = TransferUiMetrics::new(phase.clone(), bytes_complete, bytes_total);
        } else {
            entry.bytes_complete = bytes_complete;
            entry.bytes_total = bytes_total;
        }
        entry
    }

    fn transfer_metrics(&self, channel_id: &ChannelId) -> Option<&TransferUiMetrics> {
        self.transfer_metrics.get(channel_id)
    }

    fn clear_transfer_tracking(&mut self, channel_id: &ChannelId) {
        self.transfer_metrics.remove(channel_id);
        self.clear_active_progress_if_matches(channel_id);
    }
}

#[derive(Debug, Clone)]
struct FileChannelUiState {
    item_kind: TransferItemKind,
    name: String,
    size: Option<u64>,
    is_outgoing: bool,
    transfer_ready: bool,
    accepted: bool,
    accepted_by: Option<String>,
}

#[derive(Debug, Clone)]
struct TransferUiMetrics {
    phase: TransferPhase,
    started_at: Instant,
    bytes_complete: u64,
    bytes_total: Option<u64>,
}

impl TransferUiMetrics {
    fn new(phase: TransferPhase, bytes_complete: u64, bytes_total: Option<u64>) -> Self {
        Self {
            phase,
            started_at: Instant::now(),
            bytes_complete,
            bytes_total,
        }
    }
}

fn apply_room_event(
    state: &mut RoomTuiState,
    ui: &mut UiState,
    event: MachineEvent,
) -> Option<MachineCommand> {
    match &event {
        MachineEvent::RoomWelcome {
            room_id,
            self_member,
            host_member,
            ..
        } => {
            ui.stream_id = room_id.as_str().to_string();
            ui.local_name = state.local_name.clone();
            ui.set_room_identity(self_member.as_str(), host_member.as_str());
        }
        MachineEvent::MemberSnapshot { members } => {
            ui.replace_room_members(
                members
                    .iter()
                    .map(|member| (member.member_id.as_str().to_string(), member.name.clone())),
            );
        }
        MachineEvent::MemberJoined { member } => {
            ui.upsert_room_member(member.member_id.as_str(), member.name.clone());
        }
        MachineEvent::MemberLeft { member_id, .. } => {
            ui.remove_room_member(member_id.as_str());
        }
        _ => {}
    }
    let mut progress_lines = update_transfer_status_line(state, ui, &event);
    let (mut lines, follow_up) = format_room_event_lines(state, event, false);
    progress_lines.append(&mut lines);
    apply_room_lines(ui, progress_lines);
    follow_up
}

fn update_transfer_status_line(
    state: &mut RoomTuiState,
    ui: &mut UiState,
    event: &MachineEvent,
) -> Vec<RoomLine> {
    match event {
        MachineEvent::TransferProgress {
            channel_id,
            phase,
            item_kind,
            name,
            bytes_complete,
            bytes_total,
            ..
        } => {
            let elapsed = {
                let metrics =
                    state.record_transfer_progress(channel_id, phase, *bytes_complete, *bytes_total);
                metrics.started_at.elapsed()
            };
            state.set_active_progress_channel(channel_id.clone());
            let progress =
                format_progress_with_speed(*bytes_complete, *bytes_total, elapsed).unwrap_or_default();
            if matches!(phase, TransferPhase::Preparing)
                && bytes_total.is_some_and(|total| *bytes_complete >= total)
                && state
                    .file_channels
                    .get(channel_id)
                    .is_some_and(|channel| channel.is_outgoing)
            {
                let status = match state.file_channels.get(channel_id) {
                    Some(channel) if channel.accepted => channel
                        .accepted_by
                        .as_ref()
                        .map(|accepter| {
                            format!(
                                "accepted by {}; finishing preparation {} ({})",
                                accepter,
                                item_kind_label(item_kind.clone()),
                                name
                            )
                        })
                        .unwrap_or_else(|| {
                            format!(
                                "starting transfer {} ({})",
                                item_kind_label(item_kind.clone()),
                                name
                            )
                        }),
                    _ => format!("waiting for accept {} ({})", item_kind_label(item_kind.clone()), name),
                };
                ui.set_status(status);
                return Vec::new();
            }
            if matches!(phase, TransferPhase::Preparing) {
                if let Some(channel) = state.file_channels.get(channel_id) {
                    if channel.is_outgoing && channel.accepted {
                        if let Some(accepter) = &channel.accepted_by {
                            ui.set_status(format!(
                                "accepted by {}; finishing preparation {} ({}) {}",
                                accepter,
                                item_kind_label(item_kind.clone()),
                                name,
                                progress
                            ));
                            return Vec::new();
                        }
                    }
                }
            }
            ui.set_status(format!(
                "{} {} ({}) {}",
                progress_label(state, channel_id, phase),
                item_kind_label(item_kind.clone()),
                name,
                progress
            ));
            if matches!(phase, TransferPhase::Downloading)
                && bytes_total.is_some_and(|total| *bytes_complete >= total)
                && state
                    .file_channels
                    .get(channel_id)
                    .is_some_and(|channel| channel.is_outgoing)
            {
                let summary = transfer_completion_summary(
                    "sent",
                    item_kind.clone(),
                    name,
                    *bytes_complete,
                    elapsed,
                );
                state.clear_transfer_tracking(channel_id);
                ui.clear_status();
                return vec![RoomLine::System(summary)];
            }
        }
        MachineEvent::ChannelPathReceived { channel_id, .. } => {
            if state.active_progress_channel.as_ref() == Some(channel_id) {
                state.clear_active_progress_if_matches(channel_id);
                ui.clear_status();
            }
        }
        MachineEvent::ChannelAccepted {
            channel_id,
            member_id,
            member_name,
            ..
        } => {
            if let Some(channel) = state.file_channels.get(channel_id).filter(|channel| channel.is_outgoing) {
                let accepter = state.display_member_name_with_fallback(member_name, member_id);
                let status = if state
                    .transfer_metrics(channel_id)
                    .is_some_and(|metrics| matches!(metrics.phase, TransferPhase::Preparing))
                    || !channel.transfer_ready
                {
                    format!(
                        "accepted by {}; finishing preparation {} ({})",
                        accepter,
                        item_kind_label(channel.item_kind.clone()),
                        channel.name
                    )
                } else {
                    format!(
                        "accepted by {}; starting transfer {} ({})",
                        accepter,
                        item_kind_label(channel.item_kind.clone()),
                        channel.name
                    )
                };
                ui.set_status(status);
            }
        }
        MachineEvent::ChannelRejected { channel_id, .. }
        | MachineEvent::ChannelClosed { channel_id, .. } => {
            if state.active_progress_channel.as_ref() == Some(channel_id) {
                state.clear_transfer_tracking(channel_id);
                ui.clear_status();
            }
        }
        MachineEvent::Error {
            channel_id: Some(channel_id),
            ..
        } => {
            if state.active_progress_channel.as_ref() == Some(channel_id) {
                state.clear_transfer_tracking(channel_id);
                ui.clear_status();
            }
        }
        _ => {}
    }
    Vec::new()
}

fn format_room_event_lines(
    state: &mut RoomTuiState,
    event: MachineEvent,
    include_transfer_progress_lines: bool,
) -> (Vec<RoomLine>, Option<MachineCommand>) {
    match event {
        MachineEvent::RoomWelcome {
            room_id,
            self_member,
            host_member,
            ..
        } => {
            state.self_member = Some(self_member.clone());
            state.host_member = Some(host_member.clone());
            (
                vec![RoomLine::System(format!(
                    "joined room {} as {} (host {})",
                    room_id.as_str(),
                    self_member.as_str(),
                    host_member.as_str()
                ))],
                None,
            )
        }
        MachineEvent::MemberSnapshot { members } => {
            state.members = members
                .into_iter()
                .map(|member| (member.member_id, member.name))
                .collect();
            (vec![RoomLine::System(state.member_roster_line())], None)
        }
        MachineEvent::MemberJoined { member } => {
            state
                .members
                .insert(member.member_id.clone(), member.name.clone());
            (
                vec![RoomLine::System(format!(
                    "member joined: {}",
                    state.display_member(&member.member_id)
                ))],
                None,
            )
        }
        MachineEvent::MemberLeft { member_id, reason } => {
            let name = state.display_member(&member_id);
            state.members.remove(&member_id);
            (
                vec![RoomLine::System(format!(
                    "member left: {}{}",
                    name,
                    reason
                        .as_deref()
                        .map(|value| format!(" - {value}"))
                        .unwrap_or_default()
                ))],
                None,
            )
        }
        MachineEvent::RoomClosed { reason } => (
            vec![RoomLine::System(format!("room closed: {reason}"))],
            None,
        ),
        MachineEvent::Chat {
            from,
            from_name,
            to,
            text,
            ..
        } => {
            let speaker = state.display_member_name_with_fallback(&from_name, &from);
            let target = match to {
                Route::All => None,
                Route::Member { member_id } => Some(state.display_member(&member_id)),
            };
            if let Some(target) = target {
                (
                    vec![RoomLine::Chat {
                        speaker: format!("{speaker} -> {target}"),
                        text,
                    }],
                    None,
                )
            } else {
                (vec![RoomLine::Chat { speaker, text }], None)
            }
        }
        MachineEvent::ChannelOpened {
            channel_id,
            kind,
            from,
            from_name,
            to,
            name,
            size,
            transfer,
            ..
        } => {
            let route = match &to {
                Route::All => "all".to_string(),
                Route::Member { member_id } => state.display_member(member_id),
            };
            if matches!(kind, ChannelKind::File) {
                let item_kind = transfer
                    .as_ref()
                    .map(|transfer| transfer.item_kind.clone())
                    .unwrap_or(TransferItemKind::File);
                let display_name = name
                    .clone()
                    .unwrap_or_else(|| channel_id.as_str().to_string());
                let is_incoming = state
                    .self_member
                    .as_ref()
                    .is_some_and(|self_member| match &to {
                        Route::All => from_name != state.local_name,
                        Route::Member { member_id } => member_id == self_member,
                    });
                state.file_channels.insert(
                    channel_id.clone(),
                    FileChannelUiState {
                        item_kind: item_kind.clone(),
                        name: display_name.clone(),
                        size,
                        is_outgoing: !is_incoming,
                        transfer_ready: transfer
                            .as_ref()
                            .is_some_and(|offer| offer.integrity.is_some()),
                        accepted: false,
                        accepted_by: None,
                    },
                );
                if is_incoming
                    && !state
                        .pending_incoming_files
                        .iter()
                        .any(|id| id == &channel_id)
                {
                    state.pending_incoming_files.push(channel_id.clone());
                }
                let size_suffix = size
                    .map(format_bytes)
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default();
                let mut message = format!(
                    "{} wants to send {} {}{}",
                    state.display_member_name_with_fallback(&from_name, &from),
                    item_kind_label(item_kind),
                    display_name,
                    size_suffix
                );
                if is_incoming {
                    message.push_str(" - /accept or /reject");
                }
                return (vec![RoomLine::System(message)], None);
            }
            (
                vec![RoomLine::System(format!(
                    "channel {} opened by {} kind={:?} to {}",
                    channel_id.as_str(),
                    from_name,
                    kind,
                    route
                ))],
                None,
            )
        }
        MachineEvent::ChannelAccepted {
            channel_id,
            member_name,
            member_id,
            ..
        } => {
            let accepter = state.display_member_name_with_fallback(&member_name, &member_id);
            if let Some(channel) = state.file_channels.get_mut(&channel_id) {
                channel.accepted = true;
                channel.accepted_by = Some(accepter.clone());
            }
            if state.self_member.as_ref() == Some(&member_id) {
                state.pending_incoming_files.retain(|id| id != &channel_id);
            }
            let label = state
                .channel_summary(&channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            (
                vec![RoomLine::System(format!("{accepter} accepted {label}"))],
                None,
            )
        }
        MachineEvent::ChannelTransferReady {
            channel_id,
            size,
            transfer,
        } => {
            if let Some(channel) = state.file_channels.get_mut(&channel_id) {
                channel.size = size;
                channel.transfer_ready = transfer.integrity.is_some();
            }
            (Vec::new(), None)
        }
        MachineEvent::ChannelRejected {
            channel_id,
            member_name,
            member_id,
            reason,
            ..
        } => {
            state.clear_transfer_tracking(&channel_id);
            if state.self_member.as_ref() == Some(&member_id) {
                state.pending_incoming_files.retain(|id| id != &channel_id);
            }
            let label = state
                .channel_summary(&channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            state.file_channels.remove(&channel_id);
            let member = state.display_member_name_with_fallback(&member_name, &member_id);
            (
                vec![RoomLine::System(format!(
                    "{} rejected {label}{}",
                    member,
                    reason
                        .as_deref()
                        .map(|value| format!(" - {value}"))
                        .unwrap_or_default()
                ))],
                None,
            )
        }
        MachineEvent::ChannelData {
            channel_id,
            from,
            from_name,
            body,
            ..
        } => (
            vec![RoomLine::Chat {
                speaker: format!(
                    "{} [{}]",
                    state.display_member_name_with_fallback(&from_name, &from),
                    channel_id.as_str()
                ),
                text: body,
            }],
            None,
        ),
        MachineEvent::ChannelClosed {
            channel_id,
            member_id,
            member_name,
            reason,
            ..
        } => {
            state.clear_transfer_tracking(&channel_id);
            state.pending_incoming_files.retain(|id| id != &channel_id);
            let label = state
                .channel_summary(&channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            state.file_channels.remove(&channel_id);
            let member = state.display_member_name_with_fallback(&member_name, &member_id);
            (
                vec![RoomLine::System(format!(
                    "{label} closed by {}{}",
                    member,
                    reason
                        .as_deref()
                        .map(|value| format!(" - {value}"))
                        .unwrap_or_default()
                ))],
                None,
            )
        }
        MachineEvent::ChannelPathReceived {
            channel_id,
            path,
            size,
        } => {
            let transfer_elapsed = state
                .transfer_metrics(&channel_id)
                .filter(|metrics| matches!(metrics.phase, TransferPhase::Downloading))
                .map(|metrics| metrics.started_at.elapsed());
            state.clear_transfer_tracking(&channel_id);
            let label = state
                .channel_summary(&channel_id)
                .unwrap_or_else(|| channel_id.as_str().to_string());
            state.pending_incoming_files.retain(|id| id != &channel_id);
            (
                vec![RoomLine::System(format!(
                    "{label} saved to {} ({})",
                    display_path(&path),
                    transfer_completion_detail(size, transfer_elapsed)
                ))],
                None,
            )
        }
        MachineEvent::TransferProgress {
            channel_id,
            phase,
            item_kind,
            name,
            bytes_complete,
            bytes_total,
            ..
        } => {
            if matches!(phase, TransferPhase::Preparing) {
                if !include_transfer_progress_lines {
                    return (Vec::new(), None);
                }
                return (
                    vec![RoomLine::System(format!(
                        "{} {} ({}) {}",
                        progress_label(state, &channel_id, &phase),
                        item_kind_label(item_kind),
                        name,
                        format_progress(bytes_complete, bytes_total).unwrap_or_default()
                    ))],
                    None,
                );
            }
            if matches!(phase, TransferPhase::Exporting) {
                if bytes_complete == 0 {
                    let label = state.channel_summary(&channel_id).unwrap_or_else(|| {
                        format!("{} {}", item_kind_label(item_kind.clone()), name)
                    });
                    let path = unique_path_in_dir(&state.download_dir, &name);
                    return (
                        vec![RoomLine::System(format!(
                            "saving {label} to {}",
                            display_path(&path.display().to_string())
                        ))],
                        None,
                    );
                }
                if !include_transfer_progress_lines {
                    return (Vec::new(), None);
                }
            } else if !include_transfer_progress_lines {
                return (Vec::new(), None);
            }
            (
                vec![RoomLine::System(format!(
                    "{} {} ({}) {}",
                    progress_label(state, &channel_id, &phase),
                    item_kind_label(item_kind),
                    name,
                    format_progress(bytes_complete, bytes_total).unwrap_or_default()
                ))],
                None,
            )
        }
        MachineEvent::Error {
            code,
            message,
            channel_id,
        } => (
            vec![RoomLine::System(format!(
                "error {}{}: {}",
                code,
                channel_id
                    .as_ref()
                    .map(|id| format!(" ({})", id.as_str()))
                    .unwrap_or_default(),
                message
            ))],
            None,
        ),
    }
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

fn scripted_mode() -> bool {
    std::env::var_os("SKYFFLA_TUI_SCRIPTED").is_some()
}

fn summarize_room_event(
    state: &mut RoomTuiState,
    event: MachineEvent,
) -> (Vec<String>, Option<MachineCommand>) {
    let (lines, follow_up) = format_room_event_lines(state, event, true);
    (stringify_room_lines(lines), follow_up)
}

fn room_help_lines() -> &'static [&'static str] {
    &[
        "commands:",
        "plain text  broadcast chat to the room",
        "/msg <member_id> <text>  direct message one member",
        "/send <all|member_id> <path>  send a file or folder",
        "/send <name> <path>  send to a named room member",
        "/send <path>  send to the only other room member in a 1:1 room",
        "/accept [channel_id]  accept the newest pending transfer",
        "/reject [channel_id] [reason]  reject the newest pending transfer",
        "accepted transfers save automatically into the download directory",
        "/members  print the current roster",
        "/help  show this help",
        "/quit  leave the room",
        "advanced: /channel ..., /chat ...",
    ]
}

#[cfg(test)]
mod tests {
    use skyffla_protocol::room::{Member, MACHINE_PROTOCOL_VERSION};

    use super::*;

    #[test]
    fn parses_room_chat_and_dm_commands() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        match parse_room_tui_input("hello room", &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::SendChat { to, text }) => {
                assert_eq!(to, Route::All);
                assert_eq!(text, "hello room");
            }
            other => panic!("unexpected input: {other:?}"),
        }

        state
            .members
            .insert(MemberId::new("m2").unwrap(), "beta".into());
        state.self_member = Some(MemberId::new("m1").unwrap());
        match parse_room_tui_input("/msg m2 hi beta", &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::SendChat { to, text }) => {
                assert_eq!(
                    to,
                    Route::Member {
                        member_id: MemberId::new("m2").unwrap()
                    }
                );
                assert_eq!(text, "hi beta");
            }
            other => panic!("unexpected input: {other:?}"),
        }

        state
            .members
            .insert(MemberId::new("m2").unwrap(), "beta".into());
        state.self_member = Some(MemberId::new("m1").unwrap());
        match parse_room_tui_input("/msg beta hi by name", &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::SendChat { to, text }) => {
                assert_eq!(
                    to,
                    Route::Member {
                        member_id: MemberId::new("m2").unwrap()
                    }
                );
                assert_eq!(text, "hi by name");
            }
            other => panic!("unexpected input: {other:?}"),
        }
    }

    #[test]
    fn room_member_resolution_prefers_name_but_accepts_id() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        state.self_member = Some(MemberId::new("m1").unwrap());
        state
            .members
            .insert(MemberId::new("m1").unwrap(), "alpha".into());
        state
            .members
            .insert(MemberId::new("m2").unwrap(), "beta".into());
        state
            .members
            .insert(MemberId::new("m3").unwrap(), "beta".into());

        assert_eq!(
            state.resolve_member("m2").unwrap(),
            MemberId::new("m2").unwrap()
        );
        assert!(state.resolve_member("beta").is_err());
    }

    #[test]
    fn room_member_display_hides_ids_until_names_collide() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        state.self_member = Some(MemberId::new("m1").unwrap());
        state.host_member = Some(MemberId::new("m1").unwrap());
        state
            .members
            .insert(MemberId::new("m1").unwrap(), "alpha".into());
        state
            .members
            .insert(MemberId::new("m2").unwrap(), "beta".into());
        assert_eq!(state.member_roster_line(), "members: alpha*, beta");

        state
            .members
            .insert(MemberId::new("m3").unwrap(), "beta".into());
        assert_eq!(
            state.display_member(&MemberId::new("m2").unwrap()),
            "beta (m2)"
        );
        assert_eq!(
            state.display_member(&MemberId::new("m3").unwrap()),
            "beta (m3)"
        );
    }

    #[test]
    fn parses_room_file_commands() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        match parse_room_tui_input(r#"/send all "./folder name""#, &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::SendPath {
                channel_id,
                to,
                path,
                ..
            }) => {
                assert_eq!(channel_id.as_str(), "file-1");
                assert_eq!(to, Route::All);
                assert_eq!(path, "./folder name");
            }
            other => panic!("unexpected input: {other:?}"),
        }

        state
            .members
            .insert(MemberId::new("m2").unwrap(), "beta".into());
        state.self_member = Some(MemberId::new("m1").unwrap());
        match parse_room_tui_input(r#"/send "./solo.txt""#, &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::SendPath { to, path, .. }) => {
                assert_eq!(
                    to,
                    Route::Member {
                        member_id: MemberId::new("m2").unwrap()
                    }
                );
                assert_eq!(path, "./solo.txt");
            }
            other => panic!("unexpected input: {other:?}"),
        }

        state
            .pending_incoming_files
            .push(ChannelId::new("f9").unwrap());
        state.file_channels.insert(
            ChannelId::new("f9").unwrap(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "demo.txt".into(),
                size: Some(4),
                is_outgoing: false,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );
        match parse_room_tui_input("/accept", &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::AcceptChannel { channel_id }) => {
                assert_eq!(channel_id.as_str(), "f9");
            }
            other => panic!("unexpected input: {other:?}"),
        }

        match parse_room_tui_input("/reject", &mut state).unwrap() {
            RoomTuiInput::Send(MachineCommand::RejectChannel { channel_id, reason }) => {
                assert_eq!(channel_id.as_str(), "f9");
                assert_eq!(reason, None);
            }
            other => panic!("unexpected input: {other:?}"),
        }
    }

    #[test]
    fn room_events_update_roster_and_identity() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::RoomWelcome {
                protocol_version: MACHINE_PROTOCOL_VERSION,
                room_id: skyffla_protocol::room::RoomId::new("demo-room").unwrap(),
                self_member: MemberId::new("m2").unwrap(),
                host_member: MemberId::new("m1").unwrap(),
            },
        );
        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::MemberSnapshot {
                members: vec![
                    Member {
                        member_id: MemberId::new("m1").unwrap(),
                        name: "host".into(),
                        fingerprint: None,
                    },
                    Member {
                        member_id: MemberId::new("m2").unwrap(),
                        name: "alpha".into(),
                        fingerprint: None,
                    },
                ],
            },
        );

        assert_eq!(ui.self_member_id.as_deref(), Some("m2"));
        assert_eq!(ui.host_member_id.as_deref(), Some("m1"));
        assert_eq!(ui.room_members.get("m1").map(String::as_str), Some("host"));
        assert_eq!(ui.room_members.get("m2").map(String::as_str), Some("alpha"));
    }

    #[test]
    fn rejects_legacy_save_command_in_room_tui() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let error = parse_room_tui_input("/save", &mut state).unwrap_err();
        assert!(error
            .to_string()
            .contains("accepted transfers save automatically"));
    }

    #[test]
    fn help_lines_match_auto_save_flow() {
        let help = room_help_lines().join("\n");
        assert!(help.contains("accepted transfers save automatically"));
        assert!(!help.contains("/save"));
    }

    #[test]
    fn accepting_provisional_file_shows_waiting_message() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "big.bin".into(),
                size: Some(32),
                is_outgoing: false,
                transfer_ready: false,
                accepted: false,
                accepted_by: None,
            },
        );

        let lines = local_command_feedback_lines(
            &mut state,
            &MachineCommand::AcceptChannel { channel_id },
        );

        assert_eq!(
            stringify_room_lines(lines),
            vec!["accepted file big.bin 32B; waiting for sender to finish preparing"]
        );
    }

    #[test]
    fn interactive_tui_keeps_transfer_progress_out_of_event_log() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id: channel_id.clone(),
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Downloading,
                name: "report.txt".into(),
                bytes_complete: 8,
                bytes_total: Some(16),
            },
        );

        assert_eq!(ui.event_count(), 0);
        assert_eq!(
            ui.status_text(),
            Some("downloading file (report.txt) 8B / 16B (50%)")
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::ChannelPathReceived {
                channel_id,
                path: "./downloads/report.txt".into(),
                size: 16,
            },
        );

        assert_eq!(ui.status_text(), None);
        assert_eq!(ui.event_count(), 1);
    }

    #[test]
    fn interactive_tui_labels_outgoing_transfer_progress_as_sending() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "report.txt".into(),
                size: Some(16),
                is_outgoing: true,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id,
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Downloading,
                name: "report.txt".into(),
                bytes_complete: 8,
                bytes_total: Some(16),
            },
        );

        assert_eq!(
            ui.status_text(),
            Some("sending file (report.txt) 8B / 16B (50%)")
        );
        assert_eq!(ui.event_count(), 0);
    }

    #[test]
    fn interactive_tui_shows_preparing_status() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "big.bin".into(),
                size: Some(32),
                is_outgoing: true,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id,
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Preparing,
                name: "big.bin".into(),
                bytes_complete: 16,
                bytes_total: Some(32),
            },
        );

        assert_eq!(
            ui.status_text(),
            Some("preparing file (big.bin) 16B / 32B (50%)")
        );
        assert_eq!(ui.event_count(), 0);
    }

    #[test]
    fn interactive_tui_shows_waiting_for_accept_after_prepare_completes() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "big.bin".into(),
                size: Some(32),
                is_outgoing: true,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id,
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Preparing,
                name: "big.bin".into(),
                bytes_complete: 32,
                bytes_total: Some(32),
            },
        );

        assert_eq!(ui.status_text(), Some("waiting for accept file (big.bin)"));
    }

    #[test]
    fn interactive_tui_shows_starting_transfer_if_accept_arrived_during_prepare() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "big.bin".into(),
                size: Some(32),
                is_outgoing: true,
                transfer_ready: true,
                accepted: true,
                accepted_by: Some("beta".into()),
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id,
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Preparing,
                name: "big.bin".into(),
                bytes_complete: 32,
                bytes_total: Some(32),
            },
        );

        assert_eq!(
            ui.status_text(),
            Some("accepted by beta; finishing preparation file (big.bin)")
        );
    }

    #[test]
    fn interactive_tui_updates_sender_status_immediately_on_accept_during_prepare() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        state.members.insert(MemberId::new("m2").unwrap(), "beta".into());
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "big.bin".into(),
                size: Some(32),
                is_outgoing: true,
                transfer_ready: false,
                accepted: false,
                accepted_by: None,
            },
        );
        state.transfer_metrics.insert(
            channel_id.clone(),
            TransferUiMetrics {
                phase: TransferPhase::Preparing,
                started_at: Instant::now(),
                bytes_complete: 16,
                bytes_total: Some(32),
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::ChannelAccepted {
                channel_id,
                member_id: MemberId::new("m2").unwrap(),
                member_name: "beta".into(),
            },
        );

        assert_eq!(
            ui.status_text(),
            Some("accepted by beta; finishing preparation file (big.bin)")
        );
    }

    #[test]
    fn interactive_tui_logs_sender_completion_and_clears_status() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "report.txt".into(),
                size: Some(16),
                is_outgoing: true,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::TransferProgress {
                channel_id,
                item_kind: TransferItemKind::File,
                phase: TransferPhase::Downloading,
                name: "report.txt".into(),
                bytes_complete: 16,
                bytes_total: Some(16),
            },
        );

        assert_eq!(ui.status_text(), None);
        assert_eq!(ui.event_count(), 1);
    }

    #[test]
    fn transfer_completion_summary_formats_sender_speed() {
        assert_eq!(
            transfer_completion_summary(
                "sent",
                TransferItemKind::File,
                "transfer-test-2g-random.bin",
                2_147_483_648,
                Duration::from_secs(32),
            ),
            "sent file transfer-test-2g-random.bin (2048.0MiB in 32s at 64.0MiB/s)"
        );
    }

    #[test]
    fn receive_completion_keeps_speed_summary() {
        let mut state = RoomTuiState::new("alpha", PathBuf::from("."));
        let mut ui = UiState::new("demo-room", "alpha", "room").unwrap();
        let channel_id = ChannelId::new("f1").unwrap();
        state.file_channels.insert(
            channel_id.clone(),
            FileChannelUiState {
                item_kind: TransferItemKind::File,
                name: "report.txt".into(),
                size: Some(16),
                is_outgoing: false,
                transfer_ready: true,
                accepted: false,
                accepted_by: None,
            },
        );
        state.active_progress_channel = Some(channel_id.clone());
        state.transfer_metrics.insert(
            channel_id.clone(),
            TransferUiMetrics {
                phase: TransferPhase::Downloading,
                started_at: Instant::now() - Duration::from_secs(2),
                bytes_complete: 16,
                bytes_total: Some(16),
            },
        );

        apply_room_event(
            &mut state,
            &mut ui,
            MachineEvent::ChannelPathReceived {
                channel_id,
                path: "./downloads/report.txt".into(),
                size: 16,
            },
        );

        assert_eq!(ui.status_text(), None);
        assert_eq!(
            ui.last_event_text(),
            Some("file report.txt 16B saved to downloads/report.txt (16B in 2s at 8B/s)")
        );
    }
}

fn item_kind_label(kind: TransferItemKind) -> &'static str {
    match kind {
        TransferItemKind::File => "file",
        TransferItemKind::Folder => "folder",
    }
}

fn progress_label(state: &RoomTuiState, channel_id: &ChannelId, phase: &TransferPhase) -> &'static str {
    match phase {
        TransferPhase::Preparing => "preparing",
        TransferPhase::Downloading => {
            if state
                .file_channels
                .get(channel_id)
                .is_some_and(|channel| channel.is_outgoing)
            {
                "sending"
            } else {
                "downloading"
            }
        }
        TransferPhase::Exporting => "saving",
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

fn transfer_completion_summary(
    verb: &str,
    item_kind: TransferItemKind,
    name: &str,
    size: u64,
    elapsed: Duration,
) -> String {
    format!(
        "{} {} {} ({})",
        verb,
        item_kind_label(item_kind),
        name,
        transfer_completion_detail(size, Some(elapsed))
    )
}

fn transfer_completion_detail(size: u64, elapsed: Option<Duration>) -> String {
    match elapsed.and_then(|elapsed| format_duration_and_rate(size, elapsed)) {
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

fn format_rate(bytes: u64, elapsed: Duration) -> Option<String> {
    if elapsed < Duration::from_secs(1) {
        return None;
    }
    let bytes_per_second = bytes as f64 / elapsed.as_secs_f64();
    if !bytes_per_second.is_finite() || bytes_per_second <= 0.0 {
        return None;
    }
    Some(format!("{}/s", format_bytes(bytes_per_second.round() as u64)))
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

fn unique_path_in_dir(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let source = Path::new(name);
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(name);
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());

    for index in 1.. {
        let file_name = match extension {
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None => format!("{stem} ({index})"),
        };
        let candidate = dir.join(file_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("unique path search should always terminate")
}

fn path_item_kind(path: &str) -> TransferItemKind {
    let expanded = if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(rest) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    if expanded.is_dir() {
        TransferItemKind::Folder
    } else {
        TransferItemKind::File
    }
}
