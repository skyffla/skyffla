use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    BlobRef, ChannelId, ChannelKind, MachineCommand, MachineEvent, Member, MemberId, RoomId, Route,
    MACHINE_PROTOCOL_VERSION,
};
use skyffla_protocol::room_link::{AuthorityLinkMessage, PeerLinkMessage};
use skyffla_session::room::{RoomEngine, RoutedEvent};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{
    ConnectionStatus, ImportedPath, IrohConnection, IrohTransport, PeerTicket,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::app::identity::load_or_create_identity;
use crate::app::sink::EventSink;
use crate::app::trust::remember_peer;
use crate::cli_error::CliError;
use crate::config::SessionConfig;
use crate::local_state::local_state_file_path;
use crate::net::framing::{read_framed, write_framed};
use crate::runtime::handshake::exchange_hello;

#[derive(Debug)]
struct JoinState {
    self_member: Option<MemberId>,
    host_member: Option<MemberId>,
    members: BTreeMap<MemberId, Member>,
    member_tickets: BTreeMap<MemberId, String>,
    channels: BTreeMap<ChannelId, JoinChannelState>,
    peer_links: BTreeMap<MemberId, mpsc::UnboundedSender<PeerLinkMessage>>,
    local_name: String,
    local_fingerprint: Option<String>,
    local_ticket: String,
    pending_host_ticket: Option<String>,
}

#[derive(Debug, Clone)]
struct JoinChannelState {
    opener: MemberId,
    kind: ChannelKind,
    participants: BTreeSet<MemberId>,
    blob: Option<BlobRef>,
    local_file_ready: bool,
}

#[derive(Debug, Default)]
struct HostState {
    ready_file_channels: BTreeSet<ChannelId>,
}

#[derive(Debug)]
struct ConnectedPeer {
    peer: SessionPeer,
    connection_status: ConnectionStatus,
    authority_send: SendStream,
    authority_recv: RecvStream,
    connection: IrohConnection,
}

#[derive(Debug)]
struct PeerHandle {
    authority_sender: mpsc::UnboundedSender<AuthorityLinkMessage>,
    peer_sender: Option<mpsc::UnboundedSender<PeerLinkMessage>>,
    member: Member,
    ticket: Option<String>,
}

#[derive(Debug)]
enum HostInput {
    LocalCommand(MachineCommand),
    LocalInputClosed,
    PeerConnected(ConnectedPeer),
    PeerLinkReady {
        member_id: MemberId,
        sender: mpsc::UnboundedSender<PeerLinkMessage>,
    },
    PeerCommand {
        member_id: MemberId,
        command: MachineCommand,
    },
    PeerEvent {
        event: MachineEvent,
    },
    PeerDisconnected {
        member_id: MemberId,
    },
    PeerProtocolError {
        message: String,
    },
    LocalEvent {
        event: MachineEvent,
    },
}

#[derive(Debug)]
enum JoinPeerInput {
    Connected {
        member: Member,
        sender: mpsc::UnboundedSender<PeerLinkMessage>,
    },
    Event {
        member_id: MemberId,
        event: MachineEvent,
        strict_sender: bool,
    },
    Disconnected {
        member_id: MemberId,
    },
    ProtocolError {
        message: String,
    },
    LocalEvent {
        event: MachineEvent,
    },
}

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
            if strict_sender && !event_matches_sender(&member_id, &event) {
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

fn spawn_accept_loop(
    config: &SessionConfig,
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    local_fingerprint: Option<String>,
    local_ticket: String,
) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move {
        loop {
            let connection = match transport.accept_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: error.to_string(),
                    });
                    break;
                }
            };

            if let Err(error) = transport.enforce_connection_policy(&connection).await {
                let _ = host_tx.send(HostInput::PeerProtocolError {
                    message: error.to_string(),
                });
                continue;
            }

            match accept_machine_peer(
                &config,
                &transport,
                connection,
                local_fingerprint.as_deref(),
                Some(local_ticket.as_str()),
            )
            .await
            {
                Ok(connected) => {
                    if host_tx.send(HostInput::PeerConnected(connected)).is_err() {
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
    })
}

async fn accept_machine_peer(
    config: &SessionConfig,
    transport: &IrohTransport,
    connection: IrohConnection,
    local_fingerprint: Option<&str>,
    local_ticket: Option<&str>,
) -> Result<ConnectedPeer, CliError> {
    let (mut authority_send, mut authority_recv) = connection
        .accept_control_stream()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let peer = exchange_hello(
        config,
        &config.stream_id,
        &mut authority_send,
        &mut authority_recv,
        local_fingerprint,
        local_ticket,
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;
    let connection_status = transport.connection_status(&connection).await;

    Ok(ConnectedPeer {
        peer,
        connection_status,
        authority_send,
        authority_recv,
        connection,
    })
}

fn spawn_link_writer<T>(
    mut send: SendStream,
    mut rx: mpsc::UnboundedReceiver<T>,
    label: &'static str,
) -> tokio::task::JoinHandle<()>
where
    T: serde::Serialize + Send + Sync + 'static,
{
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if write_framed(&mut send, &message, label).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    })
}

fn spawn_host_authority_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_authority_link_message(&mut recv).await {
                Ok(Some(AuthorityLinkMessage::MachineCommand { command })) => {
                    if host_tx
                        .send(HostInput::PeerCommand {
                            member_id: member_id.clone(),
                            command,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(AuthorityLinkMessage::MachineEvent { event })) => {
                    if host_tx.send(HostInput::PeerEvent { event }).is_err() {
                        break;
                    }
                }
                Ok(Some(AuthorityLinkMessage::PeerConnect { .. })) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: format!(
                            "peer {} sent unexpected peer introduction to host",
                            member_id.as_str()
                        ),
                    });
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Ok(None) => {
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Err(error) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: error.to_string(),
                    });
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
            }
        }
    })
}

fn spawn_host_peer_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_peer_link_message(&mut recv).await {
                Ok(Some(PeerLinkMessage::MachineEvent { event })) => {
                    if !event_matches_sender(&member_id, &event) {
                        let _ = host_tx.send(HostInput::PeerProtocolError {
                            message: format!(
                                "peer {} delivered an event with mismatched sender",
                                member_id.as_str()
                            ),
                        });
                        let _ = host_tx.send(HostInput::PeerDisconnected {
                            member_id: member_id.clone(),
                        });
                        break;
                    }
                    if host_tx.send(HostInput::PeerEvent { event }).is_err() {
                        break;
                    }
                }
                Ok(Some(other)) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: format!(
                            "peer {} sent unexpected peer-link message {other:?}",
                            member_id.as_str()
                        ),
                    });
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Ok(None) => {
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Err(error) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: error.to_string(),
                    });
                    let _ = host_tx.send(HostInput::PeerDisconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
            }
        }
    })
}

fn spawn_join_accept_loop(
    config: &SessionConfig,
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    local_fingerprint: Option<String>,
    local_ticket: String,
) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    tokio::spawn(async move {
        loop {
            let connection = match transport.accept_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: error.to_string(),
                    });
                    break;
                }
            };

            if let Err(error) = transport.enforce_connection_policy(&connection).await {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
                continue;
            }

            match accept_peer_link(
                &config,
                connection,
                local_fingerprint.as_deref(),
                Some(local_ticket.as_str()),
            )
            .await
            {
                Ok((peer, send, recv)) => {
                    spawn_inbound_peer_link(peer, send, recv, peer_tx.clone());
                }
                Err(error) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: error.to_string(),
                    });
                }
            }
        }
    })
}

fn spawn_join_host_peer_accept(
    connection: IrohConnection,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match connection.accept_data_stream().await {
            Ok((send, mut recv)) => match read_peer_link_message(&mut recv).await {
                Ok(Some(PeerLinkMessage::PeerHello { member })) => {
                    let (sender, rx) = mpsc::unbounded_channel();
                    spawn_link_writer(send, rx, "peer link message");
                    spawn_join_peer_reader(recv, member.member_id.clone(), peer_tx.clone(), false);
                    let _ = peer_tx.send(JoinPeerInput::Connected { member, sender });
                }
                Ok(Some(other)) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: format!("expected host peer introduction, got {other:?}"),
                    });
                }
                Ok(None) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: "host closed peer link before sending introduction".into(),
                    });
                }
                Err(error) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: error.to_string(),
                    });
                }
            },
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
            }
        }
    })
}

fn spawn_host_peer_link(
    connection: IrohConnection,
    host_member: Member,
    member_id: MemberId,
    host_tx: mpsc::UnboundedSender<HostInput>,
) {
    tokio::spawn(async move {
        let (mut send, recv) = match connection.open_data_stream().await {
            Ok(streams) => streams,
            Err(error) => {
                let _ = host_tx.send(HostInput::PeerProtocolError {
                    message: error.to_string(),
                });
                return;
            }
        };
        if let Err(error) = write_framed(
            &mut send,
            &PeerLinkMessage::PeerHello {
                member: host_member,
            },
            "peer link message",
        )
        .await
        {
            let _ = host_tx.send(HostInput::PeerProtocolError {
                message: error.to_string(),
            });
            return;
        }
        let (sender, rx) = mpsc::unbounded_channel();
        spawn_link_writer(send, rx, "peer link message");
        spawn_host_peer_reader(recv, member_id.clone(), host_tx.clone());
        let _ = host_tx.send(HostInput::PeerLinkReady { member_id, sender });
    });
}

async fn accept_peer_link(
    config: &SessionConfig,
    connection: IrohConnection,
    local_fingerprint: Option<&str>,
    local_ticket: Option<&str>,
) -> Result<(SessionPeer, SendStream, RecvStream), CliError> {
    let (mut send, mut recv) = connection
        .accept_control_stream()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let peer = exchange_hello(
        config,
        &config.stream_id,
        &mut send,
        &mut recv,
        local_fingerprint,
        local_ticket,
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;
    Ok((peer, send, recv))
}

fn spawn_inbound_peer_link(
    peer: SessionPeer,
    send: SendStream,
    mut recv: RecvStream,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) {
    tokio::spawn(async move {
        match read_peer_link_message(&mut recv).await {
            Ok(Some(PeerLinkMessage::PeerHello { member })) => {
                if peer.peer_name != member.name {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: format!(
                            "peer introduction name mismatch: hello={} intro={}",
                            peer.peer_name, member.name
                        ),
                    });
                    return;
                }
                let (sender, rx) = mpsc::unbounded_channel();
                spawn_link_writer(send, rx, "peer link message");
                spawn_join_peer_reader(recv, member.member_id.clone(), peer_tx.clone(), true);
                let _ = peer_tx.send(JoinPeerInput::Connected { member, sender });
            }
            Ok(Some(other)) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: format!("expected peer introduction, got {other:?}"),
                });
            }
            Ok(None) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: "peer closed before sending introduction".into(),
                });
            }
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
            }
        }
    });
}

fn spawn_outbound_peer_link(
    config: &SessionConfig,
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    target: Member,
    ticket: String,
    local_member: Member,
    local_fingerprint: Option<String>,
    local_ticket: String,
) {
    let config = config.clone();
    tokio::spawn(async move {
        let peer_ticket = PeerTicket { encoded: ticket };
        let connection = match transport.connect(&peer_ticket).await {
            Ok(connection) => connection,
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
                return;
            }
        };

        let (mut send, mut recv) = match connection.open_control_stream().await {
            Ok(streams) => streams,
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
                return;
            }
        };

        if let Err(error) = exchange_hello(
            &config,
            &config.stream_id,
            &mut send,
            &mut recv,
            local_fingerprint.as_deref(),
            Some(local_ticket.as_str()),
        )
        .await
        {
            let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                message: error.to_string(),
            });
            return;
        }

        if let Err(error) = write_framed(
            &mut send,
            &PeerLinkMessage::PeerHello {
                member: local_member,
            },
            "peer link message",
        )
        .await
        {
            let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                message: error.to_string(),
            });
            return;
        }

        let (sender, rx) = mpsc::unbounded_channel();
        spawn_link_writer(send, rx, "peer link message");
        spawn_join_peer_reader(recv, target.member_id.clone(), peer_tx.clone(), true);
        let _ = peer_tx.send(JoinPeerInput::Connected {
            member: target,
            sender,
        });
    });
}

fn spawn_join_peer_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    strict_sender: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_peer_link_message(&mut recv).await {
                Ok(Some(PeerLinkMessage::MachineEvent { event })) => {
                    if peer_tx
                        .send(JoinPeerInput::Event {
                            member_id: member_id.clone(),
                            event,
                            strict_sender,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(other)) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: format!(
                            "peer {} sent unexpected room link message {other:?}",
                            member_id.as_str()
                        ),
                    });
                    let _ = peer_tx.send(JoinPeerInput::Disconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Ok(None) => {
                    let _ = peer_tx.send(JoinPeerInput::Disconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
                Err(error) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: error.to_string(),
                    });
                    let _ = peer_tx.send(JoinPeerInput::Disconnected {
                        member_id: member_id.clone(),
                    });
                    break;
                }
            }
        }
    })
}

fn spawn_join_blob_download(
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    blob: BlobRef,
) {
    tokio::spawn(async move {
        match transport.fetch_blob(&provider, &blob).await {
            Ok(()) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::ChannelFileReady { channel_id, blob },
                });
            }
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "blob_fetch_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

fn spawn_host_blob_download(
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    blob: BlobRef,
) {
    tokio::spawn(async move {
        match transport.fetch_blob(&provider, &blob).await {
            Ok(()) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::ChannelFileReady { channel_id, blob },
                });
            }
            Err(error) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "blob_fetch_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

fn introduce_member_to_existing_peers(
    new_peer: &PeerHandle,
    peers: &BTreeMap<MemberId, PeerHandle>,
) -> Result<(), CliError> {
    let new_ticket = new_peer
        .ticket
        .clone()
        .ok_or_else(|| CliError::protocol("new peer missing ticket"))?;
    for existing in peers.values() {
        let existing_ticket = existing
            .ticket
            .clone()
            .ok_or_else(|| CliError::protocol("existing peer missing ticket"))?;
        send_link_message(
            &new_peer.authority_sender,
            AuthorityLinkMessage::PeerConnect {
                member: existing.member.clone(),
                ticket: existing_ticket,
                connect: false,
            },
        )?;
        send_link_message(
            &existing.authority_sender,
            AuthorityLinkMessage::PeerConnect {
                member: new_peer.member.clone(),
                ticket: new_ticket.clone(),
                connect: true,
            },
        )?;
    }
    Ok(())
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

fn apply_machine_event(state: &mut JoinState, event: &MachineEvent) {
    track_join_state(state, event);
    match event {
        MachineEvent::MemberSnapshot { members } => {
            state.members = members
                .iter()
                .cloned()
                .map(|member| (member.member_id.clone(), member))
                .collect();
        }
        MachineEvent::MemberJoined { member } => {
            state
                .members
                .insert(member.member_id.clone(), member.clone());
        }
        MachineEvent::MemberLeft { member_id, .. } => {
            state.members.remove(member_id);
            state.member_tickets.remove(member_id);
            state.peer_links.remove(member_id);
            prune_join_channels_for_member(state, member_id);
        }
        MachineEvent::ChannelOpened {
            channel_id,
            kind,
            from,
            to,
            blob,
            ..
        } => {
            if let Ok(participants) = event_participants(state, from, to) {
                state.channels.insert(
                    channel_id.clone(),
                    JoinChannelState {
                        opener: from.clone(),
                        kind: kind.clone(),
                        participants,
                        blob: blob.clone(),
                        local_file_ready: state.self_member.as_ref() == Some(from),
                    },
                );
            }
        }
        MachineEvent::ChannelFileReady { channel_id, .. } => {
            if let Some(channel) = state.channels.get_mut(channel_id) {
                channel.local_file_ready = true;
            }
        }
        MachineEvent::ChannelClosed { channel_id, .. } => {
            if let Some(member_id) = event_actor_member(event) {
                remove_join_channel_participant(state, channel_id, &member_id);
            } else {
                state.channels.remove(channel_id);
            }
        }
        MachineEvent::ChannelRejected { channel_id, .. } => {
            if let Some(member_id) = event_actor_member(event) {
                remove_join_channel_participant(state, channel_id, &member_id);
            } else {
                state.channels.remove(channel_id);
            }
        }
        _ => {}
    }
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
            let path_ref = Path::new(&path);
            let ImportedPath { blob, size } = transport
                .import_path(path_ref)
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            let name = name.or_else(|| {
                path_ref
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
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
            if let Some((provider, blob)) =
                join_pending_file_download(state, &self_member, &channel_id)?
            {
                spawn_join_blob_download(transport.clone(), peer_tx, provider, channel_id, blob);
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
            let blob = join_exportable_blob(state, &self_member, &channel_id)?;
            let size = transport
                .export_path(&blob, Path::new(&path))
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            emit_event(
                stdout,
                &MachineEvent::ChannelFileExported {
                    channel_id,
                    path,
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

fn join_chat_recipients(
    self_member: &MemberId,
    route: &Route,
    members: &BTreeMap<MemberId, Member>,
) -> Result<Vec<MemberId>, CliError> {
    match route {
        Route::All => Ok(members
            .keys()
            .filter(|member_id| *member_id != self_member)
            .cloned()
            .collect()),
        Route::Member { member_id } => {
            if !members.contains_key(member_id) {
                return Err(CliError::runtime(format!(
                    "unknown room member {}",
                    member_id.as_str()
                )));
            }
            if member_id == self_member {
                return Ok(vec![]);
            }
            Ok(vec![member_id.clone()])
        }
    }
}

fn event_participants(
    state: &JoinState,
    sender: &MemberId,
    route: &Route,
) -> Result<BTreeSet<MemberId>, CliError> {
    let mut participants = BTreeSet::from([sender.clone()]);
    match route {
        Route::All => {
            for member_id in state.members.keys() {
                participants.insert(member_id.clone());
            }
        }
        Route::Member { member_id } => {
            if !state.members.contains_key(member_id) {
                return Err(CliError::runtime(format!(
                    "unknown room member {}",
                    member_id.as_str()
                )));
            }
            participants.insert(member_id.clone());
        }
    }
    Ok(participants)
}

fn join_channel_recipients(
    self_member: &MemberId,
    channel_id: &ChannelId,
    channels: &BTreeMap<ChannelId, JoinChannelState>,
) -> Result<Vec<MemberId>, CliError> {
    let channel = channels
        .get(channel_id)
        .ok_or_else(|| CliError::runtime(format!("unknown channel {}", channel_id.as_str())))?;
    if channel.kind == ChannelKind::File {
        return Err(CliError::runtime(format!(
            "channel {} is blob-backed and does not accept inline channel data",
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
    Ok(channel
        .participants
        .iter()
        .filter(|member_id| *member_id != self_member)
        .cloned()
        .collect())
}

fn join_pending_file_download(
    state: &JoinState,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<Option<(PeerTicket, BlobRef)>, CliError> {
    let Some(channel) = state.channels.get(channel_id) else {
        return Err(CliError::runtime(format!(
            "unknown channel {}",
            channel_id.as_str()
        )));
    };
    if channel.kind != ChannelKind::File || channel.local_file_ready {
        return Ok(None);
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    let blob = channel.blob.clone().ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })?;
    let ticket = state
        .member_tickets
        .get(&channel.opener)
        .cloned()
        .ok_or_else(|| {
            CliError::runtime(format!(
                "no ticket known for file provider {}",
                channel.opener.as_str()
            ))
        })?;
    Ok(Some((PeerTicket { encoded: ticket }, blob)))
}

fn join_exportable_blob(
    state: &JoinState,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<BlobRef, CliError> {
    let channel = state
        .channels
        .get(channel_id)
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
    if !channel.local_file_ready {
        return Err(CliError::runtime(format!(
            "file channel {} is not ready yet",
            channel_id.as_str()
        )));
    }
    channel.blob.clone().ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })
}

fn host_pending_file_download(
    room: &RoomEngine,
    peers: &BTreeMap<MemberId, PeerHandle>,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<Option<(PeerTicket, BlobRef)>, CliError> {
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
    Ok(Some((PeerTicket { encoded: ticket }, blob)))
}

fn host_exportable_blob(
    room: &RoomEngine,
    self_member: &MemberId,
    channel_id: &ChannelId,
    host_state: &HostState,
) -> Result<BlobRef, CliError> {
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
    channel.blob.ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })
}

fn apply_host_event(state: &mut HostState, event: &MachineEvent) {
    match event {
        MachineEvent::ChannelFileReady { channel_id, .. } => {
            state.ready_file_channels.insert(channel_id.clone());
        }
        MachineEvent::ChannelRejected { channel_id, .. }
        | MachineEvent::ChannelClosed { channel_id, .. } => {
            state.ready_file_channels.remove(channel_id);
        }
        _ => {}
    }
}

fn event_matches_sender(member_id: &MemberId, event: &MachineEvent) -> bool {
    match event {
        MachineEvent::Chat { from, .. }
        | MachineEvent::ChannelOpened { from, .. }
        | MachineEvent::ChannelData { from, .. } => from == member_id,
        MachineEvent::ChannelAccepted {
            member_id: from, ..
        }
        | MachineEvent::ChannelRejected {
            member_id: from, ..
        }
        | MachineEvent::ChannelClosed {
            member_id: from, ..
        } => from == member_id,
        MachineEvent::RoomWelcome { .. }
        | MachineEvent::MemberSnapshot { .. }
        | MachineEvent::MemberJoined { .. }
        | MachineEvent::MemberLeft { .. }
        | MachineEvent::ChannelFileReady { .. }
        | MachineEvent::ChannelFileExported { .. }
        | MachineEvent::Error { .. } => true,
    }
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
            let path_ref = Path::new(&path);
            let ImportedPath { blob, size } = transport
                .import_path(path_ref)
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            let name = name.or_else(|| {
                path_ref
                    .file_name()
                    .map(|value| value.to_string_lossy().into_owned())
            });
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
            let blob = host_exportable_blob(room, sender, &channel_id, host_state)?;
            let size = transport
                .export_path(&blob, Path::new(&path))
                .await
                .map_err(|error| CliError::transport(error.to_string()))?;
            emit_event(
                stdout,
                &MachineEvent::ChannelFileExported {
                    channel_id,
                    path,
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
                if let Some((provider, blob)) =
                    host_pending_file_download(room, peers, sender, &channel_id)?
                {
                    spawn_host_blob_download(
                        transport.clone(),
                        host_tx.clone(),
                        provider,
                        channel_id.clone(),
                        blob,
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
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
) -> Result<(), CliError> {
    for routed in routed_events {
        if &routed.recipient == host_member {
            emit_event(stdout, &routed.event).await?;
        } else if let Some(peer) = peers.get(&routed.recipient) {
            if let Some(sender) = &peer.peer_sender {
                send_link_message(
                    sender,
                    PeerLinkMessage::MachineEvent {
                        event: routed.event,
                    },
                )?;
            } else {
                emit_event(
                    stdout,
                    &local_error(
                        "peer_not_connected",
                        &format!("no direct room link to {}", routed.recipient.as_str()),
                    ),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn read_authority_link_message(
    recv: &mut RecvStream,
) -> Result<Option<AuthorityLinkMessage>, CliError> {
    read_framed::<AuthorityLinkMessage>(recv, "authority link message")
        .await
        .map_err(|error| CliError::protocol(error.to_string()))
}

async fn read_peer_link_message(
    recv: &mut RecvStream,
) -> Result<Option<PeerLinkMessage>, CliError> {
    read_framed::<PeerLinkMessage>(recv, "peer link message")
        .await
        .map_err(|error| CliError::protocol(error.to_string()))
}

fn parse_command(line: &str) -> Result<MachineCommand, CliError> {
    let command: MachineCommand =
        serde_json::from_str(line).map_err(|error| CliError::usage(error.to_string()))?;
    command.validate().map_err(protocol_error)?;
    Ok(command)
}

fn track_join_state(state: &mut JoinState, event: &MachineEvent) {
    match event {
        MachineEvent::RoomWelcome {
            self_member,
            host_member,
            ..
        } => {
            state.self_member = Some(self_member.clone());
            state.host_member = Some(host_member.clone());
            if let Some(ticket) = &state.pending_host_ticket {
                state
                    .member_tickets
                    .insert(host_member.clone(), ticket.clone());
            }
        }
        MachineEvent::MemberSnapshot { members } => {
            if let Some(host_member) = &state.host_member {
                if let Some(member) = members
                    .iter()
                    .find(|member| &member.member_id == host_member)
                {
                    state.host_member = Some(member.member_id.clone());
                }
            }
        }
        _ => {}
    }
}

fn event_actor_member(event: &MachineEvent) -> Option<MemberId> {
    match event {
        MachineEvent::ChannelAccepted { member_id, .. }
        | MachineEvent::ChannelRejected { member_id, .. }
        | MachineEvent::ChannelClosed { member_id, .. } => Some(member_id.clone()),
        _ => None,
    }
}

fn remove_join_channel_participant(
    state: &mut JoinState,
    channel_id: &ChannelId,
    member_id: &MemberId,
) {
    let self_member = state.self_member.clone();
    let should_remove = if let Some(channel) = state.channels.get_mut(channel_id) {
        channel.participants.remove(member_id);
        channel.participants.len() < 2
            || self_member
                .as_ref()
                .is_some_and(|self_member| !channel.participants.contains(self_member))
    } else {
        false
    };
    if should_remove {
        state.channels.remove(channel_id);
    }
}

fn prune_join_channels_for_member(state: &mut JoinState, member_id: &MemberId) {
    let channel_ids = state.channels.keys().cloned().collect::<Vec<_>>();
    for channel_id in channel_ids {
        remove_join_channel_participant(state, &channel_id, member_id);
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
