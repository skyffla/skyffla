use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, Member, MemberId, RoomId, Route,
};
use skyffla_protocol::room_link::AuthorityLinkMessage;
use skyffla_protocol::{ProtocolVersion, PIPE_STREAM_PROTOCOL_VERSION};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{IncomingPipeStream, IrohConnection, IrohTransport, PeerTicket};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time;

use crate::app::identity::load_or_create_identity;
use crate::app::sink::EventSink;
use crate::app::trust::remember_peer;
use crate::cli_error::CliError;
use crate::config::{AutomationMode, PipeDirection, SessionConfig};
use crate::local_state::local_state_file_path;
use crate::net::framing::write_framed;
use crate::runtime::machine_links::{
    introduce_member_to_existing_peers, read_authority_link_message, send_link_message,
    spawn_accept_loop, spawn_host_authority_reader, spawn_host_peer_link, spawn_link_writer,
    HostInput, PeerHandle,
};
use skyffla_session::room::{RoomEngine, RoomEngineError};

const PIPE_CHANNEL_ID: &str = "pipe-1";
const PIPE_CHUNK_BYTES: usize = 64 * 1024;
const SLOW_RECEIVER_FIRST_WARNING_SECS: u64 = 5;
const SLOW_RECEIVER_REPEAT_WARNING_SECS: u64 = 30;

pub(crate) async fn run_pipe_host(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: &IrohTransport,
) -> Result<(), CliError> {
    let direction = pipe_direction(config)?;
    let mut log = PipeLog::new(config.quiet);
    match direction {
        PipeDirection::Send => log.info(&format!(
            "pipe send mode: waiting for receivers in room {}",
            config.room_id
        )),
        PipeDirection::Receive => log.info(&format!(
            "pipe receive mode: waiting for a pipe stream in room {}",
            config.room_id
        )),
    }

    let identity = load_or_create_identity(&local_state_file_path())
        .map_err(|error| CliError::local_io(error.to_string()))?;
    let local_ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    let mut room = RoomEngine::new(
        RoomId::new(&config.room_id).map_err(protocol_error)?,
        config.peer_name.clone(),
        Some(identity.fingerprint.clone()),
    )
    .map_err(room_error)?;
    let host_member = room.host_member().clone();

    let (host_tx, mut host_rx) = mpsc::unbounded_channel();
    let accept_handle = spawn_accept_loop(
        config,
        transport.clone(),
        host_tx.clone(),
        Some(identity.fingerprint.clone()),
        local_ticket.encoded,
    );
    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
    let pipe_accept_handle = matches!(direction, PipeDirection::Send)
        .then(|| spawn_pipe_accept_loop(transport.clone(), pipe_tx));

    let mut peers = BTreeMap::<MemberId, PeerHandle>::new();
    let mut sender = PipeSenderState::default();
    let mut machine_state_entered = false;

    loop {
        tokio::select! {
            input = host_rx.recv() => {
                let Some(input) = input else { break; };
                if handle_host_pipe_input(
                    config,
                    sink,
                    session,
                    transport,
                    &mut log,
                    &mut room,
                    &host_member,
                    &host_tx,
                    &mut peers,
                    &mut machine_state_entered,
                    &mut sender,
                    direction,
                    input,
                ).await? {
                    break;
                }
            }
            accepted = pipe_rx.recv(), if matches!(direction, PipeDirection::Send) => {
                let Some(accepted) = accepted else { break; };
                let stream = accepted?;
                if handle_sender_pipe_stream(&mut log, &mut sender, stream).await?
                    && maybe_run_sender_stream(config, &mut log, &mut room, &host_member, &mut peers, &mut sender).await?
                {
                    break;
                }
            }
        }
    }

    accept_handle.abort();
    if let Some(handle) = pipe_accept_handle {
        handle.abort();
    }
    peers.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_pipe_join_session(
    config: &SessionConfig,
    _sink: &EventSink,
    transport: &IrohTransport,
    _connection: &IrohConnection,
    authority_send: &mut SendStream,
    authority_recv: &mut RecvStream,
    host_peer: &SessionPeer,
    _local_fingerprint: Option<String>,
    _local_ticket: String,
    host_ticket: Option<String>,
) -> Result<(), CliError> {
    let direction = pipe_direction(config)?;
    let mut log = PipeLog::new(config.quiet);
    match direction {
        PipeDirection::Send => log.info(&format!(
            "pipe send mode: joining room {} and waiting for receivers",
            config.room_id
        )),
        PipeDirection::Receive => log.info(&format!(
            "pipe receive mode: joining room {} and waiting for a pipe stream",
            config.room_id
        )),
    }

    ensure_pipe_supported(host_peer.pipe_stream_version, "host", "remote host")?;

    let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel();
    let pipe_accept_handle = matches!(direction, PipeDirection::Send)
        .then(|| spawn_pipe_accept_loop(transport.clone(), pipe_tx));

    let mut state = JoinPipeState {
        self_member: None,
        host_member: None,
        members: BTreeMap::new(),
        member_tickets: BTreeMap::new(),
        host_ticket,
        host_pipe_stream_version: host_peer.pipe_stream_version,
    };
    let mut sender = PipeSenderState::default();

    loop {
        tokio::select! {
            message = read_authority_link_message(authority_recv) => {
                let should_break = handle_join_authority_message(
                    config,
                    transport,
                    &mut log,
                    &mut state,
                    &mut sender,
                    direction,
                    authority_send,
                    message?,
                ).await?;
                if should_break {
                    break;
                }
            }
            accepted = pipe_rx.recv(), if matches!(direction, PipeDirection::Send) => {
                let Some(accepted) = accepted else { break; };
                let stream = accepted?;
                if handle_sender_pipe_stream(&mut log, &mut sender, stream).await?
                    && maybe_run_join_sender_stream(config, &mut log, &mut state, authority_send, &mut sender).await?
                {
                    break;
                }
            }
        }
    }

    if let Some(handle) = pipe_accept_handle {
        handle.abort();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_host_pipe_input(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: &IrohTransport,
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    machine_state_entered: &mut bool,
    sender: &mut PipeSenderState,
    direction: PipeDirection,
    input: HostInput,
) -> Result<bool, CliError> {
    match input {
        HostInput::PeerConnected(connected) => {
            handle_pipe_host_peer_connected(
                config,
                sink,
                session,
                host_member,
                host_tx,
                room,
                peers,
                machine_state_entered,
                connected,
            )
            .await?;
            if matches!(direction, PipeDirection::Send) {
                maybe_open_host_pipe_sender(config, log, room, host_member, peers, sender).await?;
            }
            Ok(false)
        }
        HostInput::PeerCommand { member_id, command } => {
            handle_host_peer_command(
                transport,
                log,
                room,
                host_member,
                peers,
                sender,
                direction,
                member_id,
                command,
            )
            .await
        }
        HostInput::PeerDisconnected { member_id } => {
            peers.remove(&member_id);
            let _ = room.leave(&member_id, Some("peer disconnected".into()));
            if sender.target_members.contains_key(&member_id) && !sender.completed {
                return Err(CliError::runtime(format!(
                    "pipe receiver {} disconnected before stream completed",
                    member_id.as_str()
                )));
            }
            Ok(false)
        }
        HostInput::PeerProtocolError { message } => Err(CliError::protocol(message)),
        HostInput::LocalCommand(_)
        | HostInput::LocalInputClosed
        | HostInput::PeerLinkReady { .. }
        | HostInput::PeerEvent { .. }
        | HostInput::LocalEvent { .. }
        | HostInput::PreparedTransferReady { .. }
        | HostInput::PreparedTransferFailed { .. }
        | HostInput::ReceivedFileReadyToFinalize { .. } => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_pipe_host_peer_connected(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    host_member: &MemberId,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    room: &mut RoomEngine,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    machine_state_entered: &mut bool,
    connected: crate::runtime::machine_links::ConnectedPeer,
) -> Result<(), CliError> {
    if !*machine_state_entered {
        sink.emit_runtime_event(state_changed_event(
            session
                .transition(SessionEvent::PeerConnected {
                    session_id: config.room_id.clone(),
                })
                .context("failed to record peer connection")
                .map_err(|error| CliError::runtime(error.to_string()))?,
        ));
        sink.emit_runtime_event(state_changed_event(
            session
                .transition(SessionEvent::Negotiated {
                    session_id: config.room_id.clone(),
                })
                .context("failed to record negotiated state")
                .map_err(|error| CliError::runtime(error.to_string()))?,
        ));
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
    let pipe_stream_version = connected.peer.pipe_stream_version;

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
        file_transfer_version: connected.peer.file_transfer_version,
        pipe_stream_version,
        pending_events: Vec::new(),
    };
    introduce_member_to_existing_peers(&peer_handle, peers)?;
    peers.insert(member_id.clone(), peer_handle);

    for event in join_dispatch.to_joiner {
        send_link_message(&authority_tx, AuthorityLinkMessage::MachineEvent { event })?;
    }
    deliver_authority_events(
        host_member,
        join_dispatch.to_existing_members,
        peers,
        |_| Ok(false),
    )?;

    sink.emit_runtime_event(RuntimeEvent::HandshakeCompleted {
        peer: connected.peer.clone(),
    });
    sink.emit_runtime_event(RuntimeEvent::ConnectionStatus {
        mode: connected.connection_status.mode.to_string(),
        remote_addr: connected.connection_status.remote_addr.clone(),
    });
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

#[allow(clippy::too_many_arguments)]
async fn handle_host_peer_command(
    transport: &IrohTransport,
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    sender: &mut PipeSenderState,
    direction: PipeDirection,
    member_id: MemberId,
    command: MachineCommand,
) -> Result<bool, CliError> {
    match command {
        MachineCommand::LeaveRoom => {
            peers.remove(&member_id);
            let _ = room.leave(&member_id, Some("left room".into()));
            Ok(false)
        }
        MachineCommand::OpenChannel {
            channel_id,
            kind,
            to,
            name,
            size,
            mime,
            transfer,
        } => {
            if kind != ChannelKind::Pipe {
                send_command_error_to_peer(
                    &member_id,
                    "command_failed",
                    "pipe mode only accepts pipe channels",
                    peers,
                )?;
                return Ok(false);
            }
            if let Err(error) = ensure_route_supports_pipe(host_member, peers, &to) {
                send_command_error_to_peer(
                    &member_id,
                    "command_failed",
                    &error.to_string(),
                    peers,
                )?;
                return Ok(false);
            }
            let routed = room
                .open_channel(&member_id, channel_id, kind, to, name, size, mime, transfer)
                .map_err(room_error)?;
            let mut should_break = false;
            let mut local_events = Vec::new();
            for routed in routed {
                if &routed.recipient == host_member {
                    local_events.push(routed.event);
                } else if let Some(peer) = peers.get(&routed.recipient) {
                    send_link_message(
                        &peer.authority_sender,
                        AuthorityLinkMessage::MachineEvent {
                            event: routed.event,
                        },
                    )?;
                }
            }
            for event in local_events {
                should_break |= handle_host_local_pipe_event(
                    transport,
                    log,
                    room,
                    host_member,
                    peers,
                    direction,
                    event,
                )
                .await?;
            }
            Ok(should_break)
        }
        MachineCommand::AcceptChannel { channel_id } => {
            let routed = room
                .accept_channel(&member_id, &channel_id)
                .map_err(room_error)?;
            deliver_authority_events(host_member, routed, peers, |event| {
                handle_host_sender_event(sender, event)
            })?;
            if maybe_run_sender_stream_from_host(log, room, host_member, peers, sender).await? {
                return Ok(true);
            }
            Ok(false)
        }
        MachineCommand::RejectChannel { channel_id, reason } => {
            let routed = room
                .reject_channel(&member_id, &channel_id, reason)
                .map_err(room_error)?;
            deliver_authority_events(host_member, routed, peers, |event| {
                handle_host_sender_event(sender, event)
            })?;
            if sender.channel_id.as_ref() == Some(&channel_id) {
                return Err(CliError::runtime(format!(
                    "pipe receiver {} rejected the stream",
                    member_id.as_str()
                )));
            }
            Ok(false)
        }
        MachineCommand::CloseChannel { channel_id, reason } => {
            let routed = room
                .close_channel(&member_id, &channel_id, reason)
                .map_err(room_error)?;
            deliver_authority_events(host_member, routed, peers, |event| {
                handle_host_sender_event(sender, event)
            })?;
            Ok(false)
        }
        other => {
            send_command_error_to_peer(
                &member_id,
                "command_failed",
                &format!("pipe mode does not support command {other:?}"),
                peers,
            )?;
            Ok(false)
        }
    }
}

async fn maybe_open_host_pipe_sender(
    _config: &SessionConfig,
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    sender: &mut PipeSenderState,
) -> Result<(), CliError> {
    if sender.channel_id.is_some() || peers.is_empty() {
        return Ok(());
    }
    for peer in peers.values() {
        ensure_pipe_supported(peer.pipe_stream_version, &peer.member.name, "remote peer")?;
    }
    let channel_id = ChannelId::new(PIPE_CHANNEL_ID).map_err(protocol_error)?;
    sender.channel_id = Some(channel_id.clone());
    sender.target_members = peers
        .iter()
        .map(|(member_id, peer)| (member_id.clone(), peer.member.name.clone()))
        .collect();
    let routed = room
        .open_channel(
            host_member,
            channel_id,
            ChannelKind::Pipe,
            Route::All,
            Some("pipe".into()),
            None,
            None,
            None,
        )
        .map_err(room_error)?;
    log.info(&format!(
        "opened pipe stream to {} receiver(s)",
        sender.target_members.len()
    ));
    deliver_authority_events(host_member, routed, peers, |_| Ok(false))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_host_local_pipe_event(
    transport: &IrohTransport,
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    direction: PipeDirection,
    event: MachineEvent,
) -> Result<bool, CliError> {
    if let MachineEvent::ChannelOpened {
        channel_id,
        kind: ChannelKind::Pipe,
        from,
        from_name,
        ..
    } = event
    {
        if !matches!(direction, PipeDirection::Receive) {
            let routed = room
                .reject_channel(
                    host_member,
                    &channel_id,
                    Some("peer is not in pipe receive mode".into()),
                )
                .map_err(room_error)?;
            deliver_authority_events(host_member, routed, peers, |_| Ok(false))?;
            return Ok(false);
        }
        let provider_ticket = peers
            .get(&from)
            .and_then(|peer| peer.ticket.clone())
            .ok_or_else(|| {
                CliError::protocol(format!("missing pipe sender ticket for {}", from.as_str()))
            })?;
        log.info(&format!("receiving pipe stream from {from_name}"));
        let routed = room
            .accept_channel(&host_member, &channel_id)
            .map_err(room_error)?;
        deliver_authority_events(&host_member, routed, peers, |_| Ok(false))?;
        let mut stdout = tokio::io::stdout();
        transport
            .receive_pipe_stream_to_writer(
                &PeerTicket {
                    encoded: provider_ticket,
                },
                channel_id.as_str(),
                host_member.as_str(),
                &mut stdout,
            )
            .await
            .map_err(|error| CliError::transport(error.to_string()))?;
        return Ok(true);
    }
    Ok(false)
}

fn handle_host_sender_event(
    sender: &mut PipeSenderState,
    event: MachineEvent,
) -> Result<bool, CliError> {
    match event {
        MachineEvent::ChannelAccepted {
            channel_id,
            member_id,
            ..
        } if sender.channel_id.as_ref() == Some(&channel_id) => {
            sender.accepted_members.insert(member_id);
        }
        MachineEvent::ChannelRejected {
            channel_id,
            member_id,
            reason,
            ..
        } if sender.channel_id.as_ref() == Some(&channel_id) => {
            return Err(CliError::runtime(format!(
                "pipe receiver {} rejected the stream{}",
                member_id.as_str(),
                reason.map(|value| format!(": {value}")).unwrap_or_default()
            )));
        }
        _ => {}
    }
    Ok(false)
}

async fn maybe_run_sender_stream_from_host(
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    sender: &mut PipeSenderState,
) -> Result<bool, CliError> {
    maybe_run_sender_stream(
        &SessionConfigQuiet { quiet: log.quiet },
        log,
        room,
        host_member,
        peers,
        sender,
    )
    .await
}

async fn maybe_run_sender_stream(
    config: &impl HasQuiet,
    log: &mut PipeLog,
    room: &mut RoomEngine,
    host_member: &MemberId,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    sender: &mut PipeSenderState,
) -> Result<bool, CliError> {
    if !sender.ready() {
        return Ok(false);
    }
    sender.completed = true;
    log.info("all receivers accepted pipe stream; streaming stdin");
    broadcast_stdin_to_pipe_receivers(log, sender).await?;
    if let Some(channel_id) = sender.channel_id.clone() {
        let routed = room
            .close_channel(host_member, &channel_id, Some("done".into()))
            .map_err(room_error)?;
        deliver_authority_events(host_member, routed, peers, |_| Ok(false))?;
    }
    if !config.quiet() {
        log.info("pipe stream complete");
    }
    Ok(true)
}

async fn handle_join_authority_message(
    config: &SessionConfig,
    transport: &IrohTransport,
    log: &mut PipeLog,
    state: &mut JoinPipeState,
    sender: &mut PipeSenderState,
    direction: PipeDirection,
    authority_send: &mut SendStream,
    message: Option<AuthorityLinkMessage>,
) -> Result<bool, CliError> {
    match message {
        Some(AuthorityLinkMessage::MachineEvent { event }) => {
            handle_join_machine_event(
                config,
                transport,
                log,
                state,
                sender,
                direction,
                authority_send,
                event,
            )
            .await
        }
        Some(AuthorityLinkMessage::PeerConnect { member, ticket, .. }) => {
            state.member_tickets.insert(member.member_id, ticket);
            Ok(false)
        }
        Some(AuthorityLinkMessage::MachineCommand { .. }) => Err(CliError::protocol(
            "joiner received unexpected command on authority link",
        )),
        None => Err(CliError::runtime("room closed: host left")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_join_machine_event(
    _config: &SessionConfig,
    transport: &IrohTransport,
    log: &mut PipeLog,
    state: &mut JoinPipeState,
    sender: &mut PipeSenderState,
    direction: PipeDirection,
    authority_send: &mut SendStream,
    event: MachineEvent,
) -> Result<bool, CliError> {
    match event {
        MachineEvent::RoomWelcome {
            self_member,
            host_member,
            ..
        } => {
            state.self_member = Some(self_member);
            state.host_member = Some(host_member);
            if matches!(direction, PipeDirection::Send) {
                maybe_open_join_pipe_sender(log, state, sender, authority_send).await?;
            }
            Ok(false)
        }
        MachineEvent::MemberSnapshot { members } => {
            state.members = members
                .into_iter()
                .map(|member| (member.member_id.clone(), member))
                .collect();
            if matches!(direction, PipeDirection::Send) {
                maybe_open_join_pipe_sender(log, state, sender, authority_send).await?;
            }
            Ok(false)
        }
        MachineEvent::MemberJoined { member } => {
            state.members.insert(member.member_id.clone(), member);
            if matches!(direction, PipeDirection::Send) {
                maybe_open_join_pipe_sender(log, state, sender, authority_send).await?;
            }
            Ok(false)
        }
        MachineEvent::MemberLeft { member_id, .. } => {
            state.members.remove(&member_id);
            state.member_tickets.remove(&member_id);
            if sender.target_members.contains_key(&member_id) && !sender.completed {
                return Err(CliError::runtime(format!(
                    "pipe receiver {} left before stream completed",
                    member_id.as_str()
                )));
            }
            Ok(false)
        }
        MachineEvent::ChannelOpened {
            channel_id,
            kind: ChannelKind::Pipe,
            from,
            from_name,
            ..
        } => {
            if !matches!(direction, PipeDirection::Receive) {
                send_authority_command(
                    authority_send,
                    MachineCommand::RejectChannel {
                        channel_id,
                        reason: Some("peer is not in pipe receive mode".into()),
                    },
                )
                .await?;
                return Ok(false);
            }
            let self_member = state
                .self_member
                .clone()
                .ok_or_else(|| CliError::protocol("pipe room is not ready yet"))?;
            let provider_ticket = state.provider_ticket(&from)?;
            log.info(&format!("receiving pipe stream from {from_name}"));
            send_authority_command(
                authority_send,
                MachineCommand::AcceptChannel {
                    channel_id: channel_id.clone(),
                },
            )
            .await?;
            let mut stdout = tokio::io::stdout();
            transport
                .receive_pipe_stream_to_writer(
                    &PeerTicket {
                        encoded: provider_ticket,
                    },
                    channel_id.as_str(),
                    self_member.as_str(),
                    &mut stdout,
                )
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            let _ = send_authority_command(authority_send, MachineCommand::LeaveRoom).await;
            Ok(true)
        }
        MachineEvent::ChannelAccepted {
            channel_id,
            member_id,
            ..
        } if sender.channel_id.as_ref() == Some(&channel_id) => {
            sender.accepted_members.insert(member_id);
            maybe_run_join_sender_stream(_config, log, state, authority_send, sender).await
        }
        MachineEvent::ChannelRejected {
            channel_id,
            member_id,
            reason,
            ..
        } if sender.channel_id.as_ref() == Some(&channel_id) => Err(CliError::runtime(format!(
            "pipe receiver {} rejected the stream{}",
            member_id.as_str(),
            reason.map(|value| format!(": {value}")).unwrap_or_default()
        ))),
        MachineEvent::RoomClosed { reason } => {
            Err(CliError::runtime(format!("room closed: {reason}")))
        }
        MachineEvent::Error { message, .. } => Err(CliError::runtime(message)),
        _ => Ok(false),
    }
}

async fn maybe_open_join_pipe_sender(
    log: &mut PipeLog,
    state: &mut JoinPipeState,
    sender: &mut PipeSenderState,
    authority_send: &mut SendStream,
) -> Result<(), CliError> {
    if sender.channel_id.is_some() {
        return Ok(());
    }
    ensure_pipe_supported(state.host_pipe_stream_version, "host", "remote host")?;
    let Some(self_member) = state.self_member.clone() else {
        return Ok(());
    };
    let targets = state
        .members
        .iter()
        .filter(|(member_id, _)| *member_id != &self_member)
        .map(|(member_id, member)| (member_id.clone(), member.name.clone()))
        .collect::<BTreeMap<_, _>>();
    if targets.is_empty() {
        return Ok(());
    }
    let channel_id = ChannelId::new(PIPE_CHANNEL_ID).map_err(protocol_error)?;
    sender.channel_id = Some(channel_id.clone());
    sender.target_members = targets;
    send_authority_command(
        authority_send,
        MachineCommand::OpenChannel {
            channel_id,
            kind: ChannelKind::Pipe,
            to: Route::All,
            name: Some("pipe".into()),
            size: None,
            mime: None,
            transfer: None,
        },
    )
    .await?;
    log.info(&format!(
        "opened pipe stream to {} receiver(s)",
        sender.target_members.len()
    ));
    Ok(())
}

async fn maybe_run_join_sender_stream(
    config: &SessionConfig,
    log: &mut PipeLog,
    _state: &mut JoinPipeState,
    authority_send: &mut SendStream,
    sender: &mut PipeSenderState,
) -> Result<bool, CliError> {
    if !sender.ready() {
        return Ok(false);
    }
    sender.completed = true;
    log.info("all receivers accepted pipe stream; streaming stdin");
    broadcast_stdin_to_pipe_receivers(log, sender).await?;
    if let Some(channel_id) = sender.channel_id.clone() {
        send_authority_command(
            authority_send,
            MachineCommand::CloseChannel {
                channel_id,
                reason: Some("done".into()),
            },
        )
        .await?;
    }
    if !config.quiet {
        log.info("pipe stream complete");
    }
    Ok(true)
}

async fn handle_sender_pipe_stream(
    log: &mut PipeLog,
    sender: &mut PipeSenderState,
    stream: IncomingPipeStream,
) -> Result<bool, CliError> {
    let member_id = MemberId::new(stream.member_id.clone()).map_err(protocol_error)?;
    if sender.channel_id.as_ref().map(ChannelId::as_str) != Some(stream.channel_id.as_str()) {
        return Err(CliError::protocol(format!(
            "unexpected pipe stream for channel {} from {}",
            stream.channel_id,
            member_id.as_str()
        )));
    }
    if !sender.target_members.contains_key(&member_id) {
        return Err(CliError::protocol(format!(
            "unexpected pipe receiver {} for channel {}",
            member_id.as_str(),
            stream.channel_id
        )));
    }
    log.info(&format!(
        "pipe receiver connected: {}",
        sender.display_member(&member_id)
    ));
    sender
        .streams
        .insert(member_id, PipeReceiverStream::new(stream));
    Ok(sender.ready())
}

async fn broadcast_stdin_to_pipe_receivers(
    log: &mut PipeLog,
    sender: &mut PipeSenderState,
) -> Result<(), CliError> {
    for (member_id, receiver) in sender.streams.iter_mut() {
        wait_for_receiver_credit(
            log,
            member_id,
            sender.target_members.get(member_id),
            receiver,
        )
        .await?;
    }

    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0_u8; PIPE_CHUNK_BYTES];
    loop {
        let chunk_len = sender
            .streams
            .values()
            .map(|receiver| receiver.available_credit)
            .min()
            .unwrap_or(0)
            .min(PIPE_CHUNK_BYTES as u64) as usize;
        if chunk_len == 0 {
            let blocked = sender
                .streams
                .iter_mut()
                .filter(|(_, receiver)| receiver.available_credit == 0)
                .map(|(member_id, receiver)| (member_id.clone(), receiver))
                .collect::<Vec<_>>();
            for (member_id, receiver) in blocked {
                wait_for_receiver_credit(
                    log,
                    &member_id,
                    sender.target_members.get(&member_id),
                    receiver,
                )
                .await?;
            }
            continue;
        }

        let read = stdin
            .read(&mut buffer[..chunk_len])
            .await
            .map_err(|error| CliError::local_io(format!("failed reading stdin: {error}")))?;
        if read == 0 {
            break;
        }
        for receiver in sender.streams.values_mut() {
            receiver
                .stream
                .write_all(&buffer[..read])
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            receiver.available_credit = receiver.available_credit.saturating_sub(read as u64);
        }
    }
    for receiver in sender.streams.values_mut() {
        receiver
            .stream
            .finish()
            .await
            .map_err(|error| CliError::transport(error.to_string()))?;
    }
    Ok(())
}

async fn wait_for_receiver_credit(
    log: &mut PipeLog,
    member_id: &MemberId,
    member_name: Option<&String>,
    receiver: &mut PipeReceiverStream,
) -> Result<(), CliError> {
    let mut warning_delay = Duration::from_secs(SLOW_RECEIVER_FIRST_WARNING_SECS);
    loop {
        match time::timeout(warning_delay, receiver.stream.read_credit()).await {
            Ok(Ok(credit)) => {
                receiver.available_credit = receiver.available_credit.saturating_add(credit);
                return Ok(());
            }
            Ok(Err(error)) => return Err(CliError::transport(error.to_string())),
            Err(_) => {
                log.warn(&format!(
                    "warning: {} ({}) is slowing pipe stream; blocked for {}s",
                    member_name.map(String::as_str).unwrap_or("peer"),
                    member_id.as_str(),
                    SLOW_RECEIVER_FIRST_WARNING_SECS
                ));
                warning_delay = Duration::from_secs(SLOW_RECEIVER_REPEAT_WARNING_SECS);
            }
        }
    }
}

fn spawn_pipe_accept_loop(
    transport: IrohTransport,
    tx: mpsc::UnboundedSender<Result<IncomingPipeStream, CliError>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let connection = match transport.accept_transfer_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = tx.send(Err(CliError::transport(error.to_string())));
                    break;
                }
            };
            match transport.accept_pipe_stream(connection).await {
                Ok(stream) => {
                    if tx.send(Ok(stream)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(CliError::transport(error.to_string())));
                }
            }
        }
    })
}

fn deliver_authority_events(
    host_member: &MemberId,
    routed: Vec<skyffla_session::room::RoutedEvent>,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    mut local: impl FnMut(MachineEvent) -> Result<bool, CliError>,
) -> Result<bool, CliError> {
    let mut should_break = false;
    for routed in routed {
        if &routed.recipient == host_member {
            should_break |= local(routed.event)?;
        } else if let Some(peer) = peers.get(&routed.recipient) {
            send_link_message(
                &peer.authority_sender,
                AuthorityLinkMessage::MachineEvent {
                    event: routed.event,
                },
            )?;
        }
    }
    Ok(should_break)
}

fn send_command_error_to_peer(
    member_id: &MemberId,
    code: &str,
    message: &str,
    peers: &BTreeMap<MemberId, PeerHandle>,
) -> Result<(), CliError> {
    if let Some(peer) = peers.get(member_id) {
        send_link_message(
            &peer.authority_sender,
            AuthorityLinkMessage::MachineEvent {
                event: MachineEvent::Error {
                    code: code.into(),
                    message: message.into(),
                    channel_id: None,
                },
            },
        )?;
    }
    Ok(())
}

async fn send_authority_command(
    send: &mut SendStream,
    command: MachineCommand,
) -> Result<(), CliError> {
    write_framed(
        send,
        &AuthorityLinkMessage::MachineCommand { command },
        "authority link command",
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))
}

fn ensure_route_supports_pipe(
    host_member: &MemberId,
    peers: &BTreeMap<MemberId, PeerHandle>,
    route: &Route,
) -> Result<(), CliError> {
    match route {
        Route::All => {
            for peer in peers.values() {
                ensure_pipe_supported(peer.pipe_stream_version, &peer.member.name, "remote peer")?;
            }
            Ok(())
        }
        Route::Member { member_id } if member_id == host_member => Ok(()),
        Route::Member { member_id } => {
            let peer = peers.get(member_id).ok_or_else(|| {
                CliError::runtime(format!("unknown member {}", member_id.as_str()))
            })?;
            ensure_pipe_supported(peer.pipe_stream_version, &peer.member.name, "remote peer")
        }
    }
}

fn ensure_pipe_supported(
    version: Option<ProtocolVersion>,
    peer_name: &str,
    side_label: &str,
) -> Result<(), CliError> {
    match version {
        Some(version) if version.is_compatible_with(PIPE_STREAM_PROTOCOL_VERSION) => Ok(()),
        Some(version) => Err(CliError::usage(format!(
            "cannot use pipe stream with {peer_name}: local pipe stream protocol is {}, {side_label} is {}; update the older Skyffla peer",
            PIPE_STREAM_PROTOCOL_VERSION, version
        ))),
        None => Err(CliError::usage(format!(
            "cannot use pipe stream with {peer_name}: {side_label} does not advertise pipe stream support; update the older Skyffla peer"
        ))),
    }
}

fn pipe_direction(config: &SessionConfig) -> Result<PipeDirection, CliError> {
    match config.automation {
        Some(AutomationMode::Pipe { direction }) => Ok(direction),
        _ => Err(CliError::runtime("pipe runtime started without pipe mode")),
    }
}

fn protocol_error(error: impl ToString) -> CliError {
    CliError::protocol(error.to_string())
}

fn room_error(error: RoomEngineError) -> CliError {
    CliError::protocol(error.to_string())
}

#[derive(Default)]
struct PipeSenderState {
    channel_id: Option<ChannelId>,
    target_members: BTreeMap<MemberId, String>,
    accepted_members: BTreeSet<MemberId>,
    streams: BTreeMap<MemberId, PipeReceiverStream>,
    completed: bool,
}

impl PipeSenderState {
    fn ready(&self) -> bool {
        !self.target_members.is_empty()
            && self.accepted_members.len() == self.target_members.len()
            && self.streams.len() == self.target_members.len()
            && !self.completed
    }

    fn display_member(&self, member_id: &MemberId) -> String {
        self.target_members
            .get(member_id)
            .map(|name| format!("{name} ({})", member_id.as_str()))
            .unwrap_or_else(|| member_id.as_str().to_string())
    }
}

struct PipeReceiverStream {
    stream: IncomingPipeStream,
    available_credit: u64,
}

impl PipeReceiverStream {
    fn new(stream: IncomingPipeStream) -> Self {
        Self {
            stream,
            available_credit: 0,
        }
    }
}

struct JoinPipeState {
    self_member: Option<MemberId>,
    host_member: Option<MemberId>,
    members: BTreeMap<MemberId, Member>,
    member_tickets: BTreeMap<MemberId, String>,
    host_ticket: Option<String>,
    host_pipe_stream_version: Option<ProtocolVersion>,
}

impl JoinPipeState {
    fn provider_ticket(&self, member_id: &MemberId) -> Result<String, CliError> {
        if self
            .host_member
            .as_ref()
            .is_some_and(|host_member| host_member == member_id)
        {
            return self
                .host_ticket
                .clone()
                .ok_or_else(|| CliError::protocol("missing host pipe sender ticket"));
        }
        self.member_tickets.get(member_id).cloned().ok_or_else(|| {
            CliError::protocol(format!(
                "missing pipe sender ticket for {}",
                member_id.as_str()
            ))
        })
    }
}

struct PipeLog {
    quiet: bool,
}

impl PipeLog {
    fn new(quiet: bool) -> Self {
        Self { quiet }
    }

    fn info(&self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }

    fn warn(&self, message: &str) {
        if !self.quiet {
            eprintln!("{message}");
        }
    }
}

trait HasQuiet {
    fn quiet(&self) -> bool;
}

impl HasQuiet for SessionConfig {
    fn quiet(&self) -> bool {
        self.quiet
    }
}

struct SessionConfigQuiet {
    quiet: bool,
}

impl HasQuiet for SessionConfigQuiet {
    fn quiet(&self) -> bool {
        self.quiet
    }
}
