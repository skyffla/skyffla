use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, Member, MemberId, RoomId,
    TransferItemKind, MACHINE_PROTOCOL_VERSION,
};
use skyffla_protocol::room_link::{AuthorityLinkMessage, PeerLinkMessage};
use skyffla_session::room::{RoomEngine, RoutedEvent};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{
    ConnectionStatus, ImportedPath, IrohConnection, IrohTransport, LocalTransferProgress,
    PeerTicket,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::app::identity::load_or_create_identity;
use crate::app::sink::EventSink;
use crate::app::trust::remember_peer;
use crate::cli_error::CliError;
use crate::config::SessionConfig;
use crate::local_state::local_state_file_path;
use crate::net::framing::write_framed;
use crate::runtime::machine_command_line::parse_machine_command_line;
use crate::runtime::machine_links::{
    introduce_member_to_existing_peers, peer_event_matches_sender, read_authority_link_message,
    spawn_accept_loop, spawn_host_authority_reader, spawn_host_blob_download, spawn_host_peer_link,
    spawn_join_accept_loop, spawn_join_blob_download, spawn_join_host_peer_accept,
    spawn_link_writer, spawn_outbound_peer_link, ConnectedPeer, HostInput, JoinPeerInput,
    PeerHandle,
};
use crate::runtime::machine_state::{
    apply_host_event, apply_machine_event, join_channel_recipients, join_chat_recipients,
    join_exportable_file, join_pending_file_transfer, ExportableFileTransfer, HostState,
    JoinState, PendingFileTransfer,
};

#[derive(Debug)]
enum JoinStdinInput {
    LocalCommand(MachineCommand),
    LocalInputClosed,
    LocalInputError(String),
}

enum LoopControl {
    Continue,
    Break,
}

#[derive(Debug, Clone, Default)]
struct ProgressGate {
    last_step: Option<u64>,
}

impl ProgressGate {
    fn should_emit(&mut self, progress: &LocalTransferProgress) -> bool {
        match progress.bytes_total {
            Some(0) | None => {
                let step = progress.bytes_complete / (256 * 1024);
                let emit = self.last_step != Some(step) || progress.bytes_complete == 0;
                self.last_step = Some(step);
                emit
            }
            Some(total) => {
                let step = ((progress.bytes_complete.saturating_mul(100)) / total) / 10;
                let emit = self.last_step != Some(step)
                    || progress.bytes_complete == 0
                    || progress.bytes_complete >= total;
                self.last_step = Some(step);
                emit
            }
        }
    }
}

pub(crate) async fn run_machine_host(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: &IrohTransport,
) -> Result<(), CliError> {
    let identity = load_or_create_identity(&local_state_file_path())
        .map_err(|error| CliError::local_io(error.to_string()))?;
    let local_ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    let mut room = RoomEngine::new(
        RoomId::new(&config.stream_id).map_err(protocol_error)?,
        config.peer_name.clone(),
        Some(identity.fingerprint.clone()),
    )
    .map_err(room_error)?;

    let host_member = room.host_member().clone();
    let mut stdout = tokio::io::stdout();
    emit_initial_room_events(&mut stdout, &room, &host_member).await?;

    let (host_tx, mut host_rx) = mpsc::unbounded_channel();
    let stdin_handle = spawn_host_stdin_task(host_tx.clone());
    let accept_handle = spawn_accept_loop(
        config,
        transport.clone(),
        host_tx.clone(),
        Some(identity.fingerprint.clone()),
        local_ticket.encoded,
    );

    let mut machine_state_entered = false;
    let mut local_input_closed = false;
    let mut peers = BTreeMap::<MemberId, PeerHandle>::new();
    let mut host_state = HostState::default();

    while let Some(input) = host_rx.recv().await {
        if matches!(
            handle_host_input(
                config,
                sink,
                session,
                &host_member,
                &host_tx,
                &mut room,
                &mut stdout,
                &mut peers,
                &mut host_state,
                transport,
                &mut machine_state_entered,
                &mut local_input_closed,
                input,
            )
            .await?,
            LoopControl::Break
        ) {
            break;
        }
    }

    accept_handle.abort();
    stdin_handle.abort();
    peers.clear();
    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": config.stream_id,
        "role": "host",
    }));
    Ok(())
}

pub(crate) async fn run_machine_join_session(
    config: &SessionConfig,
    sink: &EventSink,
    transport: &IrohTransport,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    local_fingerprint: Option<String>,
    local_ticket: String,
    host_ticket: Option<String>,
) -> Result<(), CliError> {
    let mut stdout = tokio::io::stdout();
    let mut input_closed = false;
    let mut peer_closed = false;
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let mut join_state = JoinState {
        self_member: None,
        host_member: None,
        members: BTreeMap::new(),
        member_tickets: BTreeMap::new(),
        channels: BTreeMap::new(),
        peer_links: BTreeMap::new(),
        local_name: config.peer_name.clone(),
        local_fingerprint,
        local_ticket,
        pending_host_ticket: host_ticket,
    };

    let accept_handle = spawn_join_accept_loop(
        config,
        transport.clone(),
        peer_tx.clone(),
        join_state.local_fingerprint.clone(),
        join_state.local_ticket.clone(),
    );
    let host_peer_accept_handle = spawn_join_host_peer_accept(connection.clone(), peer_tx.clone());
    let stdin_handle = spawn_join_stdin_task(stdin_tx);

    while !(input_closed && peer_closed) {
        tokio::select! {
            input = stdin_rx.recv(), if !input_closed => {
                input_closed = handle_join_stdin_input(
                    input,
                    &mut join_state,
                    &mut stdout,
                    send,
                    transport,
                    peer_tx.clone(),
                )
                .await?;
            }
            message = read_authority_link_message(recv), if !peer_closed => {
                peer_closed = handle_join_authority_message(
                    config,
                    transport,
                    &mut join_state,
                    &mut stdout,
                    peer_tx.clone(),
                    message?,
                )
                .await?;
            }
            peer_input = peer_rx.recv() => {
                handle_join_peer_input(sink, &mut join_state, &mut stdout, peer_input).await?;
            }
        }
    }

    accept_handle.abort();
    host_peer_accept_handle.abort();
    stdin_handle.abort();

    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": config.stream_id,
        "role": "join",
    }));

    Ok(())
}

async fn emit_initial_room_events(
    stdout: &mut tokio::io::Stdout,
    room: &RoomEngine,
    host_member: &MemberId,
) -> Result<(), CliError> {
    emit_event(
        stdout,
        &MachineEvent::RoomWelcome {
            protocol_version: MACHINE_PROTOCOL_VERSION,
            room_id: room.room_id().clone(),
            self_member: host_member.clone(),
            host_member: host_member.clone(),
        },
    )
    .await?;
    emit_event(
        stdout,
        &MachineEvent::MemberSnapshot {
            members: room.members().values().cloned().collect(),
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn handle_host_input(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    host_member: &MemberId,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    room: &mut RoomEngine,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    host_state: &mut HostState,
    transport: &IrohTransport,
    machine_state_entered: &mut bool,
    local_input_closed: &mut bool,
    input: HostInput,
) -> Result<LoopControl, CliError> {
    match input {
        HostInput::LocalCommand(command) => {
            if let Err(error) = handle_host_command(
                room,
                host_member,
                command.clone(),
                stdout,
                peers,
                host_state,
                transport,
                host_tx,
                true,
            )
            .await
            {
                emit_event(
                    stdout,
                    &command_error_event(
                        "command_failed",
                        &error.to_string(),
                        command_channel_id(&command),
                    ),
                )
                .await?;
            }
            Ok(LoopControl::Continue)
        }
        HostInput::LocalInputClosed => {
            *local_input_closed = true;
            Ok(LoopControl::Continue)
        }
        HostInput::PeerConnected(connected) => {
            handle_host_peer_connected(
                config,
                sink,
                session,
                host_member,
                host_tx,
                room,
                stdout,
                peers,
                machine_state_entered,
                connected,
            )
            .await?;
            Ok(LoopControl::Continue)
        }
        HostInput::PeerLinkReady { member_id, sender } => {
            if let Some(peer) = peers.get_mut(&member_id) {
                let pending_events = std::mem::take(&mut peer.pending_events);
                for event in pending_events {
                    send_link_message(&sender, PeerLinkMessage::MachineEvent { event })?;
                }
                peer.peer_sender = Some(sender);
                sink.emit_json_event(serde_json::json!({
                    "event": "room_link_connected",
                    "member_id": member_id,
                    "member_name": peer.member.name,
                }));
            }
            Ok(LoopControl::Continue)
        }
        HostInput::PeerCommand { member_id, command } => {
            if peers.contains_key(&member_id) {
                if let Err(error) = handle_host_command(
                    room,
                    &member_id,
                    command.clone(),
                    stdout,
                    peers,
                    host_state,
                    transport,
                    host_tx,
                    false,
                )
                .await
                {
                    send_command_error_to_peer(&member_id, &command, &error.to_string(), peers)?;
                }
            }
            Ok(LoopControl::Continue)
        }
        HostInput::PeerEvent { event } => {
            apply_host_event(host_state, &event);
            emit_event(stdout, &event).await?;
            Ok(LoopControl::Continue)
        }
        HostInput::PeerDisconnected { member_id } => {
            peers.remove(&member_id);
            if let Ok(dispatch) = room.leave(&member_id, Some("peer disconnected".into())) {
                deliver_routed_events(host_member, dispatch.to_remaining_members, stdout, peers)
                    .await?;
            }
            if *local_input_closed && peers.is_empty() {
                Ok(LoopControl::Break)
            } else {
                Ok(LoopControl::Continue)
            }
        }
        HostInput::PeerProtocolError { message } => {
            emit_event(stdout, &local_error("peer_protocol_error", &message)).await?;
            Ok(LoopControl::Continue)
        }
        HostInput::LocalEvent { event } => {
            apply_host_event(host_state, &event);
            emit_event(stdout, &event).await?;
            Ok(LoopControl::Continue)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_host_peer_connected(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    host_member: &MemberId,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    room: &mut RoomEngine,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    machine_state_entered: &mut bool,
    connected: ConnectedPeer,
) -> Result<(), CliError> {
    if !*machine_state_entered {
        enter_machine_state(session, sink, &config.stream_id)?;
        *machine_state_entered = true;
    }

    let join_dispatch = room
        .join(
            connected.peer.peer_name.clone(),
            connected.peer.peer_fingerprint.clone(),
        )
        .map_err(room_error)?;
    let member = join_dispatch.member.clone();
    let member_id = member.member_id.clone();
    let ticket = connected.peer.peer_ticket.clone();

    let (authority_tx, authority_rx) = mpsc::unbounded_channel();
    spawn_link_writer(
        connected.authority_send,
        authority_rx,
        "authority link message",
    );
    spawn_host_authority_reader(connected.authority_recv, member_id.clone(), host_tx.clone());
    let peer_handle = PeerHandle {
        authority_sender: authority_tx.clone(),
        peer_sender: None,
        member,
        ticket,
        pending_events: Vec::new(),
    };
    introduce_member_to_existing_peers(&peer_handle, peers)?;
    peers.insert(member_id, peer_handle);

    for event in join_dispatch.to_joiner {
        send_link_message(&authority_tx, AuthorityLinkMessage::MachineEvent { event })?;
    }
    deliver_routed_events(
        host_member,
        join_dispatch.to_existing_members,
        stdout,
        peers,
    )
    .await?;

    emit_peer_runtime_events(sink, &connected.peer, &connected.connection_status);
    if let Some(trust) =
        remember_peer(&connected.peer).map_err(|error| CliError::local_io(error.to_string()))?
    {
        sink.emit_runtime_event(RuntimeEvent::PeerTrust {
            status: trust.status.to_string(),
            peer_name: trust.peer_name,
            peer_fingerprint: trust.peer_fingerprint,
            previous_name: trust.previous_name,
        });
    }

    let host_member_record = room
        .member(host_member)
        .cloned()
        .ok_or_else(|| CliError::runtime("host member missing from room state"))?;
    spawn_host_peer_link(
        connected.connection,
        host_member_record,
        join_dispatch.member.member_id.clone(),
        host_tx.clone(),
    );

    Ok(())
}

async fn handle_join_stdin_input(
    input: Option<JoinStdinInput>,
    join_state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    authority_send: &mut SendStream,
    transport: &IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> Result<bool, CliError> {
    match input {
        Some(JoinStdinInput::LocalCommand(command)) => {
            if join_state.self_member.is_none() {
                emit_event(
                    stdout,
                    &local_error("room_not_ready", "machine room is not ready yet"),
                )
                .await?;
                return Ok(false);
            }
            if let Err(error) = handle_join_command(
                join_state,
                authority_send,
                command.clone(),
                stdout,
                transport,
                peer_tx,
            )
            .await
            {
                emit_event(
                    stdout,
                    &command_error_event(
                        "command_failed",
                        &error.to_string(),
                        command_channel_id(&command),
                    ),
                )
                .await?;
            }
            Ok(false)
        }
        Some(JoinStdinInput::LocalInputError(message)) => {
            emit_event(stdout, &local_error("input_error", &message)).await?;
            Ok(false)
        }
        Some(JoinStdinInput::LocalInputClosed) | None => Ok(true),
    }
}

async fn handle_join_authority_message(
    config: &SessionConfig,
    transport: &IrohTransport,
    join_state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    message: Option<AuthorityLinkMessage>,
) -> Result<bool, CliError> {
    match message {
        Some(AuthorityLinkMessage::MachineEvent { event }) => {
            apply_machine_event(join_state, &event);
            emit_event(stdout, &event).await?;
            Ok(false)
        }
        Some(AuthorityLinkMessage::PeerConnect {
            member,
            ticket,
            connect,
        }) => {
            join_state
                .member_tickets
                .insert(member.member_id.clone(), ticket.clone());
            if connect
                && !join_state.peer_links.contains_key(&member.member_id)
                && join_state
                    .self_member
                    .as_ref()
                    .is_some_and(|self_member| self_member != &member.member_id)
            {
                let Some(local_member) = join_state.local_member() else {
                    emit_event(
                        stdout,
                        &local_error("room_not_ready", "machine room is not ready yet"),
                    )
                    .await?;
                    return Ok(false);
                };
                spawn_outbound_peer_link(
                    config,
                    transport.clone(),
                    peer_tx,
                    member,
                    ticket,
                    local_member,
                    join_state.local_fingerprint.clone(),
                    join_state.local_ticket.clone(),
                );
            }
            Ok(false)
        }
        Some(AuthorityLinkMessage::MachineCommand { .. }) => Err(CliError::protocol(
            "joiner received unexpected command on authority link",
        )),
        None => Ok(true),
    }
}

async fn handle_join_peer_input(
    sink: &EventSink,
    join_state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    peer_input: Option<JoinPeerInput>,
) -> Result<(), CliError> {
    match peer_input {
        Some(JoinPeerInput::Connected { member, sender }) => {
            join_state
                .members
                .entry(member.member_id.clone())
                .or_insert_with(|| member.clone());
            join_state
                .peer_links
                .insert(member.member_id.clone(), sender);
            sink.emit_json_event(serde_json::json!({
                "event": "room_link_connected",
                "member_id": member.member_id,
                "member_name": member.name,
            }));
        }
        Some(JoinPeerInput::Event {
            member_id,
            event,
            strict_sender,
        }) => {
            if strict_sender && !peer_event_matches_sender(&member_id, &event) {
                emit_event(
                    stdout,
                    &local_error(
                        "peer_protocol_error",
                        &format!(
                            "peer {} delivered an event with mismatched sender",
                            member_id.as_str()
                        ),
                    ),
                )
                .await?;
                return Ok(());
            }
            apply_machine_event(join_state, &event);
            emit_event(stdout, &event).await?;
        }
        Some(JoinPeerInput::Disconnected { member_id }) => {
            join_state.peer_links.remove(&member_id);
            sink.emit_json_event(serde_json::json!({
                "event": "room_link_disconnected",
                "member_id": member_id,
            }));
        }
        Some(JoinPeerInput::ProtocolError { message }) => {
            emit_event(stdout, &local_error("peer_protocol_error", &message)).await?;
        }
        Some(JoinPeerInput::LocalEvent { event }) => {
            apply_machine_event(join_state, &event);
            emit_event(stdout, &event).await?;
        }
        None => {}
    }
    Ok(())
}

fn spawn_host_stdin_task(host_tx: mpsc::UnboundedSender<HostInput>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_command(&line) {
                        Ok(command) => {
                            if host_tx.send(HostInput::LocalCommand(command)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = host_tx.send(HostInput::PeerProtocolError {
                                message: error.to_string(),
                            });
                        }
                    }
                }
                Ok(None) => {
                    let _ = host_tx.send(HostInput::LocalInputClosed);
                    break;
                }
                Err(error) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: format!("failed reading machine command from stdin: {error}"),
                    });
                    let _ = host_tx.send(HostInput::LocalInputClosed);
                    break;
                }
            }
        }
    })
}

fn spawn_join_stdin_task(
    join_tx: mpsc::UnboundedSender<JoinStdinInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match parse_command(&line) {
                        Ok(command) => {
                            if join_tx.send(JoinStdinInput::LocalCommand(command)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ =
                                join_tx.send(JoinStdinInput::LocalInputError(error.to_string()));
                        }
                    }
                }
                Ok(None) => {
                    let _ = join_tx.send(JoinStdinInput::LocalInputClosed);
                    break;
                }
                Err(error) => {
                    let _ = join_tx.send(JoinStdinInput::LocalInputError(format!(
                        "failed reading machine command from stdin: {error}"
                    )));
                    let _ = join_tx.send(JoinStdinInput::LocalInputClosed);
                    break;
                }
            }
        }
    })
}

fn enter_machine_state(
    session: &mut SessionMachine,
    sink: &EventSink,
    session_id: &str,
) -> Result<(), CliError> {
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::PeerConnected {
                session_id: session_id.to_string(),
            })
            .context("failed to record peer connection")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Negotiated {
                session_id: session_id.to_string(),
                session_mode: skyffla_protocol::SessionMode::Machine,
            })
            .context("failed to record negotiated state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    Ok(())
}

fn emit_peer_runtime_events(
    sink: &EventSink,
    peer: &SessionPeer,
    connection_status: &ConnectionStatus,
) {
    sink.emit_runtime_event(RuntimeEvent::HandshakeCompleted { peer: peer.clone() });
    sink.emit_runtime_event(RuntimeEvent::ConnectionStatus {
        mode: connection_status.mode.to_string(),
        remote_addr: connection_status.remote_addr.clone(),
    });
}

async fn handle_join_command(
    state: &mut JoinState,
    authority_send: &mut SendStream,
    command: MachineCommand,
    stdout: &mut tokio::io::Stdout,
    transport: &IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> Result<(), CliError> {
    command.validate().map_err(protocol_error)?;
    let self_member = state
        .self_member
        .clone()
        .ok_or_else(|| CliError::protocol("machine room is not ready yet"))?;

    match command {
        MachineCommand::SendChat { to, text } => {
            let event = MachineEvent::Chat {
                from: self_member.clone(),
                from_name: state.local_name.clone(),
                to: to.clone(),
                text,
            };
            send_join_direct_event(
                state,
                stdout,
                join_chat_recipients(&self_member, &to, &state.members)?,
                event,
            )
            .await?;
            Ok(())
        }
        MachineCommand::SendFile {
            channel_id,
            to,
            path,
            name,
            mime,
        } => {
            let expanded_path = expand_user_path(&path);
            let item_kind = if expanded_path.is_dir() {
                TransferItemKind::Folder
            } else {
                TransferItemKind::File
            };
            let display_name = expanded_path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| expanded_path.display().to_string());
            let ImportedPath { blob, size, .. } = transport
                .import_path_with_progress(&expanded_path, {
                    let channel_id = channel_id.clone();
                    let peer_tx = peer_tx.clone();
                    let item_kind = item_kind.clone();
                    let display_name = display_name.clone();
                    let mut gate = ProgressGate::default();
                    move |progress| {
                        if gate.should_emit(&progress) {
                            let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                                event: transfer_progress_event(
                                    &channel_id,
                                    item_kind.clone(),
                                    &display_name,
                                    &progress,
                                    None,
                                ),
                            });
                        }
                    }
                })
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            let name = name.or(Some(display_name));
            let open = MachineCommand::OpenChannel {
                channel_id,
                kind: ChannelKind::File,
                to,
                name,
                size: Some(size),
                mime,
                blob: Some(blob),
            };
            let MachineCommand::OpenChannel {
                channel_id,
                kind,
                to,
                name,
                size,
                mime,
                blob,
            } = open
            else {
                unreachable!("send_file should always lower to open_channel");
            };
            let local_event = MachineEvent::ChannelOpened {
                channel_id: channel_id.clone(),
                kind: kind.clone(),
                from: self_member,
                from_name: state.local_name.clone(),
                to: to.clone(),
                name: name.clone(),
                size,
                mime: mime.clone(),
                blob: blob.clone(),
            };
            send_peer_command(
                authority_send,
                &MachineCommand::OpenChannel {
                    channel_id,
                    kind,
                    to,
                    name,
                    size,
                    mime,
                    blob,
                },
            )
            .await?;
            apply_machine_event(state, &local_event);
            Ok(())
        }
        MachineCommand::SendChannelData { channel_id, body } => {
            let event = MachineEvent::ChannelData {
                channel_id: channel_id.clone(),
                from: self_member.clone(),
                from_name: state.local_name.clone(),
                body,
            };
            send_join_direct_event(
                state,
                stdout,
                join_channel_recipients(&self_member, &channel_id, &state.channels)?,
                event,
            )
            .await?;
            Ok(())
        }
        MachineCommand::OpenChannel {
            channel_id,
            kind,
            to,
            name,
            size,
            mime,
            blob,
        } => {
            let local_event = MachineEvent::ChannelOpened {
                channel_id: channel_id.clone(),
                kind: kind.clone(),
                from: self_member,
                from_name: state.local_name.clone(),
                to: to.clone(),
                name: name.clone(),
                size,
                mime: mime.clone(),
                blob: blob.clone(),
            };
            send_peer_command(
                authority_send,
                &MachineCommand::OpenChannel {
                    channel_id,
                    kind,
                    to,
                    name,
                    size,
                    mime,
                    blob,
                },
            )
            .await?;
            apply_machine_event(state, &local_event);
            Ok(())
        }
        MachineCommand::AcceptChannel { channel_id } => {
            send_peer_command(
                authority_send,
                &MachineCommand::AcceptChannel {
                    channel_id: channel_id.clone(),
                },
            )
            .await?;
            if let Some(PendingFileTransfer {
                provider_ticket,
                blob,
                item_kind,
                name,
                size,
            }) = join_pending_file_transfer(state, &self_member, &channel_id)?
            {
                spawn_join_blob_download(
                    transport.clone(),
                    peer_tx,
                    PeerTicket {
                        encoded: provider_ticket,
                    },
                    channel_id,
                    blob,
                    item_kind,
                    name,
                    size,
                );
            }
            Ok(())
        }
        MachineCommand::RejectChannel { channel_id, reason } => {
            let command = MachineCommand::RejectChannel {
                channel_id: channel_id.clone(),
                reason,
            };
            send_peer_command(authority_send, &command).await?;
            state.channels.remove(&channel_id);
            Ok(())
        }
        MachineCommand::CloseChannel { channel_id, reason } => {
            let command = MachineCommand::CloseChannel {
                channel_id: channel_id.clone(),
                reason,
            };
            send_peer_command(authority_send, &command).await?;
            state.channels.remove(&channel_id);
            Ok(())
        }
        MachineCommand::ExportChannelFile { channel_id, path } => {
            let file = join_exportable_file(state, &self_member, &channel_id)?;
            let expanded_path = expand_user_path(&path);
            let mut gate = ProgressGate::default();
            let size = transport
                .export_path_with_progress(&file.blob, &expanded_path, |progress| {
                    if gate.should_emit(&progress) {
                        let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                            event: transfer_progress_event(
                                &channel_id,
                                file.item_kind.clone(),
                                &file.name,
                                &progress,
                                file.size,
                            ),
                        });
                    }
                })
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            emit_event(
                stdout,
                &MachineEvent::ChannelFileExported {
                    channel_id,
                    path: expanded_path.display().to_string(),
                    size,
                },
            )
            .await
        }
    }
}

async fn send_join_direct_event(
    state: &JoinState,
    stdout: &mut tokio::io::Stdout,
    recipients: Vec<MemberId>,
    event: MachineEvent,
) -> Result<(), CliError> {
    for recipient in recipients {
        if let Some(peer) = state.peer_links.get(&recipient) {
            send_link_message(
                peer,
                PeerLinkMessage::MachineEvent {
                    event: event.clone(),
                },
            )?;
        } else {
            emit_event(
                stdout,
                &local_error(
                    "peer_not_connected",
                    &format!("no direct room link to {}", recipient.as_str()),
                ),
            )
            .await?;
        }
    }
    Ok(())
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

fn transfer_progress_event(
    channel_id: &ChannelId,
    item_kind: TransferItemKind,
    name: &str,
    progress: &LocalTransferProgress,
    total_override: Option<u64>,
) -> MachineEvent {
    MachineEvent::TransferProgress {
        channel_id: channel_id.clone(),
        item_kind,
        name: name.to_string(),
        phase: progress.phase.clone(),
        bytes_complete: progress.bytes_complete,
        bytes_total: total_override.or(progress.bytes_total),
    }
}

fn host_pending_file_download(
    room: &RoomEngine,
    peers: &BTreeMap<MemberId, PeerHandle>,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<Option<(PeerTicket, ExportableFileTransfer)>, CliError> {
    let Some(channel) = room.channel(channel_id) else {
        return Err(CliError::runtime(format!(
            "unknown channel {}",
            channel_id.as_str()
        )));
    };
    if channel.kind != ChannelKind::File || channel.opener == *self_member {
        return Ok(None);
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    let blob = channel.blob.ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })?;
    let ticket = peers
        .get(&channel.opener)
        .and_then(|peer| peer.ticket.clone())
        .ok_or_else(|| {
            CliError::runtime(format!(
                "no ticket known for file provider {}",
                channel.opener.as_str()
            ))
        })?;
    let item_kind = match blob.format {
        skyffla_protocol::room::BlobFormat::Blob => TransferItemKind::File,
        skyffla_protocol::room::BlobFormat::Collection => TransferItemKind::Folder,
    };
    Ok(Some((
        PeerTicket { encoded: ticket },
        ExportableFileTransfer {
            blob,
            item_kind,
            name: channel
                .name
                .unwrap_or_else(|| channel_id.as_str().to_string()),
            size: channel.size,
        },
    )))
}

fn host_exportable_file(
    room: &RoomEngine,
    self_member: &MemberId,
    channel_id: &ChannelId,
    host_state: &HostState,
) -> Result<ExportableFileTransfer, CliError> {
    let channel = room
        .channel(channel_id)
        .ok_or_else(|| CliError::runtime(format!("unknown channel {}", channel_id.as_str())))?;
    if channel.kind != ChannelKind::File {
        return Err(CliError::runtime(format!(
            "channel {} is not a file channel",
            channel_id.as_str()
        )));
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    if !host_state.ready_file_channels.contains(channel_id) {
        return Err(CliError::runtime(format!(
            "file channel {} is not ready yet",
            channel_id.as_str()
        )));
    }
    let blob = channel.blob.ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })?;
    Ok(ExportableFileTransfer {
        blob: blob.clone(),
        item_kind: match blob.format {
            skyffla_protocol::room::BlobFormat::Blob => TransferItemKind::File,
            skyffla_protocol::room::BlobFormat::Collection => TransferItemKind::Folder,
        },
        name: channel
            .name
            .unwrap_or_else(|| channel_id.as_str().to_string()),
        size: channel.size,
    })
}

async fn handle_host_command(
    room: &mut RoomEngine,
    sender: &MemberId,
    command: MachineCommand,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    host_state: &mut HostState,
    transport: &IrohTransport,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    local_command: bool,
) -> Result<(), CliError> {
    command.validate().map_err(protocol_error)?;

    match command {
        MachineCommand::SendChat { to, text } => {
            let routed = room.send_chat(sender, to, text).map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::OpenChannel {
            channel_id,
            kind,
            to,
            name,
            size,
            mime,
            blob,
        } => {
            let routed = room
                .open_channel(sender, channel_id, kind, to, name, size, mime, blob)
                .map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::RejectChannel { channel_id, reason } => {
            let routed = room
                .reject_channel(sender, &channel_id, reason)
                .map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::SendChannelData { channel_id, body } => {
            let routed = room
                .send_channel_data(sender, &channel_id, body)
                .map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::CloseChannel { channel_id, reason } => {
            let routed = room
                .close_channel(sender, &channel_id, reason)
                .map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::SendFile {
            channel_id,
            to,
            path,
            name,
            mime,
        } => {
            if !local_command {
                return Err(CliError::usage(
                    "send_file is only valid as a local machine command",
                ));
            }
            let expanded_path = expand_user_path(&path);
            let item_kind = if expanded_path.is_dir() {
                TransferItemKind::Folder
            } else {
                TransferItemKind::File
            };
            let display_name = expanded_path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| expanded_path.display().to_string());
            let mut gate = ProgressGate::default();
            let ImportedPath { blob, size, .. } = transport
                .import_path_with_progress(&expanded_path, |progress| {
                    if gate.should_emit(&progress) {
                        let _ = host_tx.send(HostInput::LocalEvent {
                            event: transfer_progress_event(
                                &channel_id,
                                item_kind.clone(),
                                &display_name,
                                &progress,
                                None,
                            ),
                        });
                    }
                })
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            let name = name.or(Some(display_name));
            let routed = room
                .open_channel(
                    sender,
                    channel_id.clone(),
                    ChannelKind::File,
                    to,
                    name,
                    Some(size),
                    mime,
                    Some(blob),
                )
                .map_err(room_error)?;
            host_state.ready_file_channels.insert(channel_id.clone());
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::ExportChannelFile { channel_id, path } => {
            if !local_command {
                return Err(CliError::usage(
                    "export_channel_file is only valid as a local machine command",
                ));
            }
            let file = host_exportable_file(room, sender, &channel_id, host_state)?;
            let expanded_path = expand_user_path(&path);
            let mut gate = ProgressGate::default();
            let size = transport
                .export_path_with_progress(&file.blob, &expanded_path, |progress| {
                    if gate.should_emit(&progress) {
                        let _ = host_tx.send(HostInput::LocalEvent {
                            event: transfer_progress_event(
                                &channel_id,
                                file.item_kind.clone(),
                                &file.name,
                                &progress,
                                file.size,
                            ),
                        });
                    }
                })
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            emit_event(
                stdout,
                &MachineEvent::ChannelFileExported {
                    channel_id,
                    path: expanded_path.display().to_string(),
                    size,
                },
            )
            .await
        }
        MachineCommand::AcceptChannel { channel_id } => {
            let routed = room
                .accept_channel(sender, &channel_id)
                .map_err(room_error)?;
            if local_command {
                if let Some((provider, file)) =
                    host_pending_file_download(room, peers, sender, &channel_id)?
                {
                    spawn_host_blob_download(
                        transport.clone(),
                        host_tx.clone(),
                        provider,
                        channel_id.clone(),
                        file.blob,
                        file.item_kind,
                        file.name,
                        file.size,
                    );
                }
            }
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
    }
}

async fn deliver_routed_events(
    host_member: &MemberId,
    routed_events: Vec<RoutedEvent>,
    _stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
) -> Result<(), CliError> {
    for routed in routed_events {
        if &routed.recipient == host_member {
            emit_event(_stdout, &routed.event).await?;
        } else if let Some(peer) = peers.get_mut(&routed.recipient) {
            if let Some(sender) = &peer.peer_sender {
                send_link_message(
                    sender,
                    PeerLinkMessage::MachineEvent {
                        event: routed.event,
                    },
                )?;
            } else {
                peer.pending_events.push(routed.event);
            }
        }
    }
    Ok(())
}

fn parse_command(line: &str) -> Result<MachineCommand, CliError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage("machine command must not be empty"));
    }
    if trimmed.starts_with('{') {
        let command: MachineCommand =
            serde_json::from_str(trimmed).map_err(|error| CliError::usage(error.to_string()))?;
        command.validate().map_err(protocol_error)?;
        Ok(command)
    } else {
        parse_machine_command_line(trimmed)
    }
}

fn local_error(code: &str, message: &str) -> MachineEvent {
    MachineEvent::Error {
        code: code.into(),
        message: message.into(),
        channel_id: None,
    }
}

fn command_error_event(code: &str, message: &str, channel_id: Option<ChannelId>) -> MachineEvent {
    MachineEvent::Error {
        code: code.into(),
        message: message.into(),
        channel_id,
    }
}

fn command_channel_id(command: &MachineCommand) -> Option<ChannelId> {
    match command {
        MachineCommand::SendChat { .. } => None,
        MachineCommand::SendFile { channel_id, .. }
        | MachineCommand::OpenChannel { channel_id, .. }
        | MachineCommand::AcceptChannel { channel_id }
        | MachineCommand::RejectChannel { channel_id, .. }
        | MachineCommand::SendChannelData { channel_id, .. }
        | MachineCommand::CloseChannel { channel_id, .. }
        | MachineCommand::ExportChannelFile { channel_id, .. } => Some(channel_id.clone()),
    }
}

async fn emit_event(stdout: &mut tokio::io::Stdout, event: &MachineEvent) -> Result<(), CliError> {
    let line =
        serde_json::to_string(event).map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .write_all(line.as_bytes())
        .await
        .context("failed writing machine event to stdout")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .write_all(b"\n")
        .await
        .context("failed writing machine event newline")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .flush()
        .await
        .context("failed flushing machine event stdout")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    Ok(())
}

async fn send_peer_command(
    send: &mut SendStream,
    command: &MachineCommand,
) -> Result<(), CliError> {
    write_framed(
        send,
        &AuthorityLinkMessage::MachineCommand {
            command: command.clone(),
        },
        "authority link command",
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))
}

fn send_link_message<T>(sender: &mpsc::UnboundedSender<T>, message: T) -> Result<(), CliError> {
    sender
        .send(message)
        .map_err(|_| CliError::protocol("room link channel closed"))
}

fn send_command_error_to_peer(
    member_id: &MemberId,
    command: &MachineCommand,
    message: &str,
    peers: &BTreeMap<MemberId, PeerHandle>,
) -> Result<(), CliError> {
    let Some(peer) = peers.get(member_id) else {
        return Ok(());
    };
    send_link_message(
        &peer.authority_sender,
        AuthorityLinkMessage::MachineEvent {
            event: command_error_event("command_failed", message, command_channel_id(command)),
        },
    )
}

fn protocol_error(error: impl ToString) -> CliError {
    CliError::protocol(error.to_string())
}

fn room_error(error: impl ToString) -> CliError {
    CliError::runtime(error.to_string())
}

impl JoinState {
    fn local_member(&self) -> Option<Member> {
        self.self_member
            .as_ref()
            .and_then(|member_id| self.members.get(member_id))
            .cloned()
    }
}
