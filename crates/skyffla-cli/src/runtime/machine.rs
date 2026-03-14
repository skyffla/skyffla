use std::collections::BTreeMap;

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    MachineCommand, MachineEvent, Member, MemberId, RoomId, Route, MACHINE_PROTOCOL_VERSION,
};
use skyffla_protocol::room_link::RoomLinkMessage;
use skyffla_session::room::{RoomEngine, RoutedEvent};
use skyffla_session::{state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer};
use skyffla_transport::{ConnectionStatus, IrohConnection, IrohTransport, PeerTicket};
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
    peer_links: BTreeMap<MemberId, mpsc::UnboundedSender<RoomLinkMessage>>,
    local_name: String,
    local_fingerprint: Option<String>,
    local_ticket: String,
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
    sender: mpsc::UnboundedSender<RoomLinkMessage>,
    member: Member,
    ticket: Option<String>,
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
    PeerEvent {
        event: MachineEvent,
    },
    PeerDisconnected {
        member_id: MemberId,
    },
    PeerProtocolError {
        message: String,
    },
}

#[derive(Debug)]
enum JoinPeerInput {
    Connected {
        member: Member,
        sender: mpsc::UnboundedSender<RoomLinkMessage>,
    },
    Event {
        member_id: MemberId,
        event: MachineEvent,
    },
    Disconnected {
        member_id: MemberId,
    },
    ProtocolError {
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
                let member = join_dispatch.member.clone();
                let member_id = member.member_id.clone();
                let ticket = connected.peer.peer_ticket.clone();

                let (peer_tx, peer_rx) = mpsc::unbounded_channel();
                spawn_peer_writer(connected.send, peer_rx);
                spawn_peer_reader(connected.recv, member_id.clone(), host_tx.clone());
                let peer_handle = PeerHandle {
                    sender: peer_tx.clone(),
                    member,
                    ticket,
                };
                introduce_member_to_existing_peers(&peer_handle, &peers)?;
                peers.insert(member_id.clone(), peer_handle);

                for event in join_dispatch.to_joiner {
                    send_link_message(&peer_tx, RoomLinkMessage::MachineEvent { event })?;
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
            HostInput::PeerEvent { event } => {
                emit_event(&mut stdout, &event).await?;
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
    config: &SessionConfig,
    sink: &EventSink,
    transport: &IrohTransport,
    send: &mut SendStream,
    recv: &mut RecvStream,
    local_fingerprint: Option<String>,
    local_ticket: String,
) -> Result<(), CliError> {
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut input_closed = false;
    let mut peer_closed = false;
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
    let mut join_state = JoinState {
        self_member: None,
        host_member: None,
        members: BTreeMap::new(),
        peer_links: BTreeMap::new(),
        local_name: config.peer_name.clone(),
        local_fingerprint,
        local_ticket,
    };

    let accept_handle = spawn_join_accept_loop(
        config,
        transport.clone(),
        peer_tx.clone(),
        join_state.local_fingerprint.clone(),
        join_state.local_ticket.clone(),
    );

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
                        handle_join_command(&mut join_state, send, command, &mut stdout).await?;
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
                        apply_authority_event(&mut join_state, &event);
                        emit_event(&mut stdout, &event).await?;
                    }
                    Some(RoomLinkMessage::PeerIntroduction { member, ticket, connect }) => {
                        if connect
                            && !join_state.peer_links.contains_key(&member.member_id)
                            && join_state.self_member.as_ref().is_some_and(|self_member| self_member != &member.member_id)
                        {
                            let Some(local_member) = join_state.local_member() else {
                                emit_event(&mut stdout, &local_error("room_not_ready", "machine room is not ready yet")).await?;
                                continue;
                            };
                            spawn_outbound_peer_link(
                                config,
                                transport.clone(),
                                peer_tx.clone(),
                                member,
                                ticket,
                                local_member,
                                join_state.local_fingerprint.clone(),
                                join_state.local_ticket.clone(),
                            );
                        }
                    }
                    Some(RoomLinkMessage::MachineCommand { .. }) => {
                        return Err(CliError::protocol(
                            "joiner received unexpected command on authority link",
                        ));
                    }
                    None => {
                        peer_closed = true;
                    }
                }
            }
            peer_input = peer_rx.recv() => {
                match peer_input {
                    Some(JoinPeerInput::Connected { member, sender }) => {
                        join_state.peer_links.insert(member.member_id.clone(), sender);
                        sink.emit_json_event(serde_json::json!({
                            "event": "room_link_connected",
                            "member_id": member.member_id,
                            "member_name": member.name,
                        }));
                    }
                    Some(JoinPeerInput::Event { member_id, event }) => {
                        if !event_matches_sender(&member_id, &event) {
                            emit_event(
                                &mut stdout,
                                &local_error(
                                    "peer_protocol_error",
                                    &format!(
                                        "peer {} delivered an event with mismatched sender",
                                        member_id.as_str()
                                    ),
                                ),
                            )
                            .await?;
                            continue;
                        }
                        emit_event(&mut stdout, &event).await?;
                    }
                    Some(JoinPeerInput::Disconnected { member_id }) => {
                        join_state.peer_links.remove(&member_id);
                        sink.emit_json_event(serde_json::json!({
                            "event": "room_link_disconnected",
                            "member_id": member_id,
                        }));
                    }
                    Some(JoinPeerInput::ProtocolError { message }) => {
                        emit_event(&mut stdout, &local_error("peer_protocol_error", &message)).await?;
                    }
                    None => {}
                }
            }
        }
    }

    accept_handle.abort();

    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": config.stream_id,
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
    mut rx: mpsc::UnboundedReceiver<RoomLinkMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if write_framed(
                &mut send,
                &message,
                "room link message",
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
                Ok(Some(RoomLinkMessage::MachineEvent { event })) => {
                    if host_tx.send(HostInput::PeerEvent { event }).is_err() {
                        break;
                    }
                }
                Ok(Some(RoomLinkMessage::PeerIntroduction { .. })) => {
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
                    spawn_inbound_peer_link(connected, peer_tx.clone());
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

fn spawn_inbound_peer_link(
    connected: ConnectedPeer,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) {
    tokio::spawn(async move {
        let ConnectedPeer {
            send,
            mut recv,
            ..
        } = connected;
        match read_room_link_message(&mut recv).await {
            Ok(Some(RoomLinkMessage::PeerIntroduction { member, .. })) => {
                let (sender, rx) = mpsc::unbounded_channel();
                spawn_peer_writer(send, rx);
                spawn_join_peer_reader(recv, member.member_id.clone(), peer_tx.clone());
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
            &RoomLinkMessage::PeerIntroduction {
                member: local_member,
                ticket: local_ticket,
                connect: false,
            },
            "room link message",
        )
        .await
        {
            let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                message: error.to_string(),
            });
            return;
        }

        let (sender, rx) = mpsc::unbounded_channel();
        spawn_peer_writer(send, rx);
        spawn_join_peer_reader(recv, target.member_id.clone(), peer_tx.clone());
        let _ = peer_tx.send(JoinPeerInput::Connected { member: target, sender });
    });
}

fn spawn_join_peer_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_room_link_message(&mut recv).await {
                Ok(Some(RoomLinkMessage::MachineEvent { event })) => {
                    if peer_tx
                        .send(JoinPeerInput::Event {
                            member_id: member_id.clone(),
                            event,
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
            &new_peer.sender,
            RoomLinkMessage::PeerIntroduction {
                member: existing.member.clone(),
                ticket: existing_ticket,
                connect: false,
            },
        )?;
        send_link_message(
            &existing.sender,
            RoomLinkMessage::PeerIntroduction {
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

fn apply_authority_event(state: &mut JoinState, event: &MachineEvent) {
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
            state.peer_links.remove(member_id);
        }
        _ => {}
    }
}

async fn handle_join_command(
    state: &mut JoinState,
    authority_send: &mut SendStream,
    command: MachineCommand,
    stdout: &mut tokio::io::Stdout,
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
            for recipient in join_chat_recipients(&self_member, &to, &state.members)? {
                if state.host_member.as_ref().is_some_and(|host| host == &recipient) {
                    write_framed(
                        authority_send,
                        &RoomLinkMessage::MachineEvent {
                            event: event.clone(),
                        },
                        "room link event",
                    )
                    .await
                    .map_err(|error| CliError::protocol(error.to_string()))?;
                } else if let Some(peer) = state.peer_links.get(&recipient) {
                    send_link_message(
                        peer,
                        RoomLinkMessage::MachineEvent {
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
        other => send_peer_command(authority_send, &other).await,
    }
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

fn event_matches_sender(member_id: &MemberId, event: &MachineEvent) -> bool {
    match event {
        MachineEvent::Chat { from, .. }
        | MachineEvent::ChannelOpened { from, .. }
        | MachineEvent::ChannelData { from, .. } => from == member_id,
        MachineEvent::ChannelAccepted { member_id: from, .. }
        | MachineEvent::ChannelRejected { member_id: from, .. }
        | MachineEvent::ChannelClosed { member_id: from, .. } => from == member_id,
        MachineEvent::RoomWelcome { .. }
        | MachineEvent::MemberSnapshot { .. }
        | MachineEvent::MemberJoined { .. }
        | MachineEvent::MemberLeft { .. }
        | MachineEvent::Error { .. } => true,
    }
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
            send_link_message(
                &peer.sender,
                RoomLinkMessage::MachineEvent {
                    event: routed.event,
                },
            )?;
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
        send_link_message(
            &peer.sender,
            RoomLinkMessage::MachineEvent {
                event,
            },
        )
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

fn send_link_message(
    sender: &mpsc::UnboundedSender<RoomLinkMessage>,
    message: RoomLinkMessage,
) -> Result<(), CliError> {
    sender
        .send(message)
        .map_err(|_| CliError::protocol("room link channel closed"))
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
