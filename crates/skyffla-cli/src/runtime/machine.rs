use std::collections::BTreeMap;

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    MachineCommand, MachineEvent, MemberId, RoomId, MACHINE_PROTOCOL_VERSION,
};
use skyffla_protocol::room_link::RoomLinkMessage;
use skyffla_session::room::{RoomEngine, RoutedEvent};
use skyffla_session::{state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer};
use skyffla_transport::{ConnectionStatus, IrohConnection, IrohTransport};
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
}

#[derive(Debug)]
struct ConnectedPeer {
    peer: SessionPeer,
    connection_status: ConnectionStatus,
    send: SendStream,
    recv: RecvStream,
}

#[derive(Debug)]
struct PeerHandle {
    sender: mpsc::UnboundedSender<MachineEvent>,
}

#[derive(Debug)]
enum HostInput {
    LocalCommand(MachineCommand),
    LocalInputClosed,
    PeerConnected(ConnectedPeer),
    PeerCommand {
        member_id: MemberId,
        command: MachineCommand,
    },
    PeerDisconnected {
        member_id: MemberId,
    },
    PeerProtocolError {
        message: String,
    },
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
    emit_event(
        &mut stdout,
        &MachineEvent::RoomWelcome {
            protocol_version: MACHINE_PROTOCOL_VERSION,
            room_id: room.room_id().clone(),
            self_member: host_member.clone(),
            host_member: host_member.clone(),
        },
    )
    .await?;
    emit_event(
        &mut stdout,
        &MachineEvent::MemberSnapshot {
            members: room.members().values().cloned().collect(),
        },
    )
    .await?;

    let (host_tx, mut host_rx) = mpsc::unbounded_channel();
    let stdin_handle = spawn_host_stdin_task(host_tx.clone());
    let accept_handle = spawn_accept_loop(
        config,
        sink,
        transport.clone(),
        host_tx.clone(),
        Some(identity.fingerprint.clone()),
        local_ticket.encoded,
    );

    let mut machine_state_entered = false;
    let mut peers = BTreeMap::<MemberId, PeerHandle>::new();

    while let Some(input) = host_rx.recv().await {
        match input {
            HostInput::LocalCommand(command) => {
                handle_host_command(
                    &mut room,
                    &host_member,
                    command,
                    &mut stdout,
                    &mut peers,
                )
                .await?;
            }
            HostInput::LocalInputClosed => break,
            HostInput::PeerConnected(connected) => {
                if !machine_state_entered {
                    enter_machine_state(session, sink, &config.stream_id)?;
                    machine_state_entered = true;
                }

                let join_dispatch = room
                    .join(
                        connected.peer.peer_name.clone(),
                        connected.peer.peer_fingerprint.clone(),
                    )
                    .map_err(room_error)?;
                let member_id = join_dispatch.member.member_id.clone();

                let (peer_tx, peer_rx) = mpsc::unbounded_channel();
                spawn_peer_writer(connected.send, peer_rx);
                spawn_peer_reader(connected.recv, member_id.clone(), host_tx.clone());
                peers.insert(member_id.clone(), PeerHandle { sender: peer_tx.clone() });

                for event in join_dispatch.to_joiner {
                    send_peer_event(&peer_tx, &event)?;
                }
                deliver_routed_events(
                    &host_member,
                    join_dispatch.to_existing_members,
                    &mut stdout,
                    &mut peers,
                )
                .await?;

                emit_peer_runtime_events(sink, &connected.peer, &connected.connection_status);
                if let Some(trust) = remember_peer(&connected.peer)
                    .map_err(|error| CliError::local_io(error.to_string()))?
                {
                    sink.emit_runtime_event(RuntimeEvent::PeerTrust {
                        status: trust.status.to_string(),
                        peer_name: trust.peer_name,
                        peer_fingerprint: trust.peer_fingerprint,
                        previous_name: trust.previous_name,
                    });
                }
            }
            HostInput::PeerCommand { member_id, command } => {
                if peers.contains_key(&member_id) {
                    handle_host_command(&mut room, &member_id, command, &mut stdout, &mut peers)
                        .await?;
                }
            }
            HostInput::PeerDisconnected { member_id } => {
                peers.remove(&member_id);
                if let Ok(dispatch) = room.leave(&member_id, Some("peer disconnected".into())) {
                    deliver_routed_events(
                        &host_member,
                        dispatch.to_remaining_members,
                        &mut stdout,
                        &mut peers,
                    )
                    .await?;
                }
            }
            HostInput::PeerProtocolError { message } => {
                emit_event(
                    &mut stdout,
                    &local_error("peer_protocol_error", &message),
                )
                .await?;
            }
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
    session_id: &str,
    sink: &EventSink,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CliError> {
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut input_closed = false;
    let mut peer_closed = false;
    let mut join_state = JoinState {
        self_member: None,
        host_member: None,
    };

    while !(input_closed && peer_closed) {
        tokio::select! {
            next_line = stdin_lines.next_line(), if !input_closed => {
                match next_line
                    .context("failed reading machine command from stdin")
                    .map_err(|error| CliError::local_io(error.to_string()))? {
                    Some(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if join_state.self_member.is_none() {
                            emit_event(&mut stdout, &local_error("room_not_ready", "machine room is not ready yet")).await?;
                            continue;
                        }
                        let command = parse_command(&line)?;
                        send_peer_command(send, &command).await?;
                    }
                    None => {
                        input_closed = true;
                        let _ = send.finish();
                    }
                }
            }
            message = read_room_link_message(recv), if !peer_closed => {
                match message? {
                    Some(RoomLinkMessage::MachineEvent { event }) => {
                        track_join_state(&mut join_state, &event);
                        emit_event(&mut stdout, &event).await?;
                    }
                    Some(RoomLinkMessage::MachineCommand { .. } | RoomLinkMessage::PeerIntroduction { .. }) => {
                        return Err(CliError::protocol(
                            "joiner received unexpected non-event room link message from peer",
                        ));
                    }
                    None => {
                        peer_closed = true;
                    }
                }
            }
        }
    }

    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": session_id,
        "role": "join",
    }));

    Ok(())
}

fn spawn_host_stdin_task(
    host_tx: mpsc::UnboundedSender<HostInput>,
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

fn spawn_accept_loop(
    config: &SessionConfig,
    sink: &EventSink,
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    local_fingerprint: Option<String>,
    local_ticket: String,
) -> tokio::task::JoinHandle<()> {
    let config = config.clone();
    let sink = sink.clone();
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
                &sink,
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
    _sink: &EventSink,
    transport: &IrohTransport,
    connection: IrohConnection,
    local_fingerprint: Option<&str>,
    local_ticket: Option<&str>,
) -> Result<ConnectedPeer, CliError> {
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
    let connection_status = transport.connection_status(&connection).await;

    Ok(ConnectedPeer {
        peer,
        connection_status,
        send,
        recv,
    })
}

fn spawn_peer_writer(
    mut send: SendStream,
    mut rx: mpsc::UnboundedReceiver<MachineEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if write_framed(
                &mut send,
                &RoomLinkMessage::MachineEvent { event },
                "room link event",
            )
            .await
            .is_err()
            {
                break;
            }
        }
        let _ = send.finish();
    })
}

fn spawn_peer_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_room_link_message(&mut recv).await {
                Ok(Some(RoomLinkMessage::MachineCommand { command })) => {
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
                Ok(Some(RoomLinkMessage::MachineEvent { .. } | RoomLinkMessage::PeerIntroduction { .. })) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: format!(
                            "peer {} sent unexpected non-command room link message",
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

async fn handle_host_command(
    room: &mut RoomEngine,
    sender: &MemberId,
    command: MachineCommand,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
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
        } => {
            let from_name = room.member_name(sender).map_err(room_error)?;
            let event = MachineEvent::ChannelOpened {
                channel_id,
                kind,
                from: sender.clone(),
                from_name,
                to: to.clone(),
                name,
                size,
                mime,
            };
            let routed = room.route_event(sender, to, event).map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::AcceptChannel { channel_id } => {
            let member_name = room.member_name(sender).map_err(room_error)?;
            deliver_channel_reply(
                sender,
                stdout,
                peers,
                MachineEvent::ChannelAccepted {
                    channel_id,
                    member_id: sender.clone(),
                    member_name,
                },
            )
            .await
        }
        MachineCommand::RejectChannel { channel_id, reason } => {
            let member_name = room.member_name(sender).map_err(room_error)?;
            deliver_channel_reply(
                sender,
                stdout,
                peers,
                MachineEvent::ChannelRejected {
                    channel_id,
                    member_id: sender.clone(),
                    member_name,
                    reason,
                },
            )
            .await
        }
        MachineCommand::SendChannelData { channel_id, body } => {
            let from_name = room.member_name(sender).map_err(room_error)?;
            deliver_channel_reply(
                sender,
                stdout,
                peers,
                MachineEvent::ChannelData {
                    channel_id,
                    from: sender.clone(),
                    from_name,
                    body,
                },
            )
            .await
        }
        MachineCommand::CloseChannel { channel_id, reason } => {
            let member_name = room.member_name(sender).map_err(room_error)?;
            deliver_channel_reply(
                sender,
                stdout,
                peers,
                MachineEvent::ChannelClosed {
                    channel_id,
                    member_id: sender.clone(),
                    member_name,
                    reason,
                },
            )
            .await
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
            send_peer_event(&peer.sender, &routed.event)?;
        }
    }
    Ok(())
}

async fn deliver_channel_reply(
    sender: &MemberId,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    event: MachineEvent,
) -> Result<(), CliError> {
    if let Some(peer) = peers.get(sender) {
        send_peer_event(&peer.sender, &event)
    } else {
        emit_event(stdout, &event).await
    }
}

async fn read_room_link_message(
    recv: &mut RecvStream,
) -> Result<Option<RoomLinkMessage>, CliError> {
    read_framed::<RoomLinkMessage>(recv, "room link message")
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
        }
        MachineEvent::MemberSnapshot { members } => {
            if let Some(host_member) = &state.host_member {
                if let Some(member) = members.iter().find(|member| &member.member_id == host_member) {
                    state.host_member = Some(member.member_id.clone());
                }
            }
        }
        _ => {}
    }
}

fn local_error(code: &str, message: &str) -> MachineEvent {
    MachineEvent::Error {
        code: code.into(),
        message: message.into(),
        channel_id: None,
    }
}

async fn emit_event(
    stdout: &mut tokio::io::Stdout,
    event: &MachineEvent,
) -> Result<(), CliError> {
    let line = serde_json::to_string(event).map_err(|error| CliError::local_io(error.to_string()))?;
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

async fn send_peer_command(send: &mut SendStream, command: &MachineCommand) -> Result<(), CliError> {
    write_framed(
        send,
        &RoomLinkMessage::MachineCommand {
            command: command.clone(),
        },
        "room link command",
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))
}

fn send_peer_event(
    sender: &mpsc::UnboundedSender<MachineEvent>,
    event: &MachineEvent,
) -> Result<(), CliError> {
    sender
        .send(event.clone())
        .map_err(|_| CliError::protocol("peer event channel closed"))
}

fn protocol_error(error: impl ToString) -> CliError {
    CliError::protocol(error.to_string())
}

fn room_error(error: impl ToString) -> CliError {
    CliError::runtime(error.to_string())
}
