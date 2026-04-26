use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineCommand, MachineEvent, Member, MemberId, RoomId, TransferDigest,
    TransferItemKind, TransferOffer, MACHINE_PROTOCOL_VERSION,
};
use skyffla_protocol::room_link::{AuthorityLinkMessage, PeerLinkMessage};
use skyffla_protocol::{ProtocolVersion, FILE_TRANSFER_PROTOCOL_VERSION};
use skyffla_session::room::{RoomEngine, RoomEngineError, RoutedEvent};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{
    ConnectionStatus, IrohConnection, IrohTransport, LocalTransferProgress, PeerTicket,
    TransferRuntimeProgress,
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
    spawn_accept_loop, spawn_host_authority_reader, spawn_host_direct_directory_receive,
    spawn_host_direct_receive, spawn_host_peer_link, spawn_host_transfer_accept_loop,
    spawn_join_accept_loop, spawn_join_direct_directory_receive, spawn_join_direct_receive,
    spawn_join_host_peer_accept, spawn_join_transfer_accept_loop, spawn_link_writer,
    spawn_outbound_peer_link, ConnectedPeer, HostInput, JoinPeerInput, PeerHandle,
};
use crate::runtime::machine_state::{
    apply_host_event, apply_machine_event, join_channel_recipients, join_chat_recipients,
    join_pending_file_transfer, HostState, JoinState, PendingFileTransfer, PendingReceivedFile,
};
use crate::runtime::transfer_progress::ProgressEmissionGate;

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

#[derive(Debug, Clone)]
struct FileTransferTarget {
    transfer: TransferOffer,
    name: String,
    size: Option<u64>,
}

#[derive(Debug, Clone)]
struct ResolvedSendPath {
    expanded_path: PathBuf,
    item_kind: TransferItemKind,
    display_name: String,
    provisional_size: Option<u64>,
}

fn spawn_join_transfer_progress_forwarder(
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> mpsc::UnboundedSender<TransferRuntimeProgress> {
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<TransferRuntimeProgress>();
    tokio::spawn(async move {
        let mut gate = ProgressEmissionGate::default();
        while let Some(progress) = progress_rx.recv().await {
            if !gate.should_emit(
                &progress.phase,
                progress.bytes_complete,
                progress.bytes_total,
            ) {
                continue;
            }
            let channel_id = match ChannelId::new(progress.channel_id) {
                Ok(channel_id) => channel_id,
                Err(_) => continue,
            };
            let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                event: MachineEvent::TransferProgress {
                    channel_id,
                    item_kind: progress.item_kind,
                    name: progress.name,
                    phase: progress.phase,
                    bytes_complete: progress.bytes_complete,
                    bytes_total: progress.bytes_total,
                },
            });
        }
    });
    progress_tx
}

fn spawn_host_transfer_progress_forwarder(
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> mpsc::UnboundedSender<TransferRuntimeProgress> {
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<TransferRuntimeProgress>();
    tokio::spawn(async move {
        let mut gate = ProgressEmissionGate::default();
        while let Some(progress) = progress_rx.recv().await {
            if !gate.should_emit(
                &progress.phase,
                progress.bytes_complete,
                progress.bytes_total,
            ) {
                continue;
            }
            let channel_id = match ChannelId::new(progress.channel_id) {
                Ok(channel_id) => channel_id,
                Err(_) => continue,
            };
            let _ = host_tx.send(HostInput::LocalEvent {
                event: MachineEvent::TransferProgress {
                    channel_id,
                    item_kind: progress.item_kind,
                    name: progress.name,
                    phase: progress.phase,
                    bytes_complete: progress.bytes_complete,
                    bytes_total: progress.bytes_total,
                },
            });
        }
    });
    progress_tx
}

fn spawn_host_prepare_path_task(
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    channel_id: ChannelId,
    path: PathBuf,
    item_kind: TransferItemKind,
    display_name: String,
) {
    tokio::spawn(async move {
        match item_kind {
            TransferItemKind::File => {
                let mut gate = ProgressEmissionGate::default();
                let prepared_result = transport
                    .prepare_file_path_with_progress(&path, |progress| {
                        if gate.should_emit(
                            &progress.phase,
                            progress.bytes_complete,
                            progress.bytes_total,
                        ) {
                            let _ = host_tx.send(HostInput::LocalEvent {
                                event: transfer_progress_event(
                                    &channel_id,
                                    TransferItemKind::File,
                                    &display_name,
                                    &progress,
                                    None,
                                ),
                            });
                        }
                    })
                    .await;
                match prepared_result {
                    Ok(prepared) => {
                        let _ = transport
                            .update_outgoing_file_hash(
                                channel_id.as_str(),
                                prepared.content_hash.clone(),
                            )
                            .await;
                        let _ = host_tx.send(HostInput::PreparedTransferReady {
                            channel_id,
                            size: prepared.size,
                            transfer: file_transfer_offer(prepared.content_hash),
                        });
                    }
                    Err(error) => {
                        let _ = host_tx.send(HostInput::PreparedTransferFailed {
                            channel_id,
                            message: error.to_string(),
                        });
                    }
                }
            }
            TransferItemKind::Folder => {
                let mut gate = ProgressEmissionGate::default();
                let prepared_result = transport
                    .prepare_directory_path_with_progress(&path, |progress| {
                        if gate.should_emit(
                            &progress.phase,
                            progress.bytes_complete,
                            progress.bytes_total,
                        ) {
                            let _ = host_tx.send(HostInput::LocalEvent {
                                event: transfer_progress_event(
                                    &channel_id,
                                    TransferItemKind::Folder,
                                    &display_name,
                                    &progress,
                                    None,
                                ),
                            });
                        }
                    })
                    .await;
                match prepared_result {
                    Ok(prepared) => {
                        let progress_tx = spawn_host_transfer_progress_forwarder(host_tx.clone());
                        transport
                            .register_outgoing_directory(
                                channel_id.as_str().to_string(),
                                &path,
                                prepared.clone(),
                                Some(progress_tx),
                            )
                            .await;
                        let _ = host_tx.send(HostInput::PreparedTransferReady {
                            channel_id,
                            size: prepared.total_size,
                            transfer: folder_transfer_offer(),
                        });
                    }
                    Err(error) => {
                        let _ = host_tx.send(HostInput::PreparedTransferFailed {
                            channel_id,
                            message: error.to_string(),
                        });
                    }
                }
            }
        }
    });
}

fn spawn_join_prepare_path_task(
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    channel_id: ChannelId,
    path: PathBuf,
    item_kind: TransferItemKind,
    display_name: String,
) {
    tokio::spawn(async move {
        match item_kind {
            TransferItemKind::File => {
                let mut gate = ProgressEmissionGate::default();
                let prepared_result = transport
                    .prepare_file_path_with_progress(&path, |progress| {
                        if gate.should_emit(
                            &progress.phase,
                            progress.bytes_complete,
                            progress.bytes_total,
                        ) {
                            let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                                event: transfer_progress_event(
                                    &channel_id,
                                    TransferItemKind::File,
                                    &display_name,
                                    &progress,
                                    None,
                                ),
                            });
                        }
                    })
                    .await;
                match prepared_result {
                    Ok(prepared) => {
                        let _ = transport
                            .update_outgoing_file_hash(
                                channel_id.as_str(),
                                prepared.content_hash.clone(),
                            )
                            .await;
                        let _ = peer_tx.send(JoinPeerInput::PreparedTransferReady {
                            channel_id,
                            size: prepared.size,
                            transfer: file_transfer_offer(prepared.content_hash),
                        });
                    }
                    Err(error) => {
                        let _ = peer_tx.send(JoinPeerInput::PreparedTransferFailed {
                            channel_id,
                            message: error.to_string(),
                        });
                    }
                }
            }
            TransferItemKind::Folder => {
                let mut gate = ProgressEmissionGate::default();
                let prepared_result = transport
                    .prepare_directory_path_with_progress(&path, |progress| {
                        if gate.should_emit(
                            &progress.phase,
                            progress.bytes_complete,
                            progress.bytes_total,
                        ) {
                            let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                                event: transfer_progress_event(
                                    &channel_id,
                                    TransferItemKind::Folder,
                                    &display_name,
                                    &progress,
                                    None,
                                ),
                            });
                        }
                    })
                    .await;
                match prepared_result {
                    Ok(prepared) => {
                        let progress_tx = spawn_join_transfer_progress_forwarder(peer_tx.clone());
                        transport
                            .register_outgoing_directory(
                                channel_id.as_str().to_string(),
                                &path,
                                prepared.clone(),
                                Some(progress_tx),
                            )
                            .await;
                        let _ = peer_tx.send(JoinPeerInput::PreparedTransferReady {
                            channel_id,
                            size: prepared.total_size,
                            transfer: folder_transfer_offer(),
                        });
                    }
                    Err(error) => {
                        let _ = peer_tx.send(JoinPeerInput::PreparedTransferFailed {
                            channel_id,
                            message: error.to_string(),
                        });
                    }
                }
            }
        }
    });
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
        RoomId::new(&config.room_id).map_err(protocol_error)?,
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
    let transfer_accept_handle =
        spawn_host_transfer_accept_loop(transport.clone(), host_tx.clone());

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
    transfer_accept_handle.abort();
    stdin_handle.abort();
    peers.clear();
    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": config.room_id,
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
    host_peer: &SessionPeer,
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
        pending_accepted_files: Default::default(),
        pending_received_files: Default::default(),
        local_name: config.peer_name.clone(),
        local_fingerprint,
        local_ticket,
        pending_host_ticket: host_ticket,
        host_file_transfer_version: host_peer.file_transfer_version,
    };

    let accept_handle = spawn_join_accept_loop(
        config,
        transport.clone(),
        peer_tx.clone(),
        join_state.local_fingerprint.clone(),
        join_state.local_ticket.clone(),
    );
    let transfer_accept_handle =
        spawn_join_transfer_accept_loop(transport.clone(), peer_tx.clone());
    let host_peer_accept_handle = spawn_join_host_peer_accept(connection.clone(), peer_tx.clone());
    let stdin_handle = spawn_join_stdin_task(stdin_tx);

    while !(input_closed && peer_closed) {
        tokio::select! {
            input = stdin_rx.recv(), if !input_closed => {
                input_closed = handle_join_stdin_input(
                    config,
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
                handle_join_peer_input(
                    sink,
                    transport,
                    &config.download_dir,
                    &mut join_state,
                    &mut stdout,
                    send,
                    peer_tx.clone(),
                    peer_input,
                )
                .await?;
            }
        }
    }

    accept_handle.abort();
    transfer_accept_handle.abort();
    host_peer_accept_handle.abort();
    stdin_handle.abort();

    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": config.room_id,
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
            if matches!(command, MachineCommand::LeaveRoom) {
                broadcast_room_closed(
                    peers,
                    MachineEvent::RoomClosed {
                        reason: "host left".into(),
                    },
                )?;
                return Ok(LoopControl::Break);
            }
            if let Err(error) = handle_host_command(
                room,
                host_member,
                command.clone(),
                &config.download_dir,
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
                if matches!(command, MachineCommand::LeaveRoom) {
                    if let Ok(dispatch) = room.leave(&member_id, Some("left room".into())) {
                        deliver_routed_events(
                            host_member,
                            dispatch.to_remaining_members,
                            stdout,
                            peers,
                        )
                        .await?;
                    }
                    peers.remove(&member_id);
                    return Ok(LoopControl::Continue);
                }
                if let Err(error) = handle_host_command(
                    room,
                    &member_id,
                    command.clone(),
                    &config.download_dir,
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
        HostInput::PreparedTransferReady {
            channel_id,
            size,
            transfer,
        } => {
            match room.update_channel_transfer(host_member, &channel_id, Some(size), transfer) {
                Ok(routed) => {
                    maybe_start_host_pending_receive(
                        room,
                        peers,
                        host_state,
                        room.host_member(),
                        &channel_id,
                        &config.download_dir,
                        transport,
                        host_tx,
                    )?;
                    deliver_routed_events(room.host_member(), routed, stdout, peers).await?;
                }
                Err(
                    RoomEngineError::UnknownChannel { .. }
                    | RoomEngineError::NotChannelOpener { .. },
                ) => {}
                Err(error) => return Err(room_error(error)),
            }
            Ok(LoopControl::Continue)
        }
        HostInput::ReceivedFileReadyToFinalize {
            channel_id,
            received,
        } => {
            host_state.pending_received_files.insert(
                channel_id.clone(),
                PendingReceivedFile {
                    temp_path: received.temp_path,
                    final_path: received.final_path,
                    actual_hash: received.content_hash,
                    size: received.size,
                },
            );
            maybe_finalize_host_received_file(room, host_state, stdout, &channel_id).await?;
            Ok(LoopControl::Continue)
        }
        HostInput::PreparedTransferFailed {
            channel_id,
            message,
        } => {
            match room.close_channel(
                host_member,
                &channel_id,
                Some("sender failed to prepare transfer".into()),
            ) {
                Ok(routed) => {
                    deliver_routed_events(room.host_member(), routed, stdout, peers).await?;
                }
                Err(
                    RoomEngineError::UnknownChannel { .. }
                    | RoomEngineError::NotChannelOpener { .. },
                ) => {}
                Err(error) => return Err(room_error(error)),
            }
            emit_event(stdout, &local_error("transfer_prepare_failed", &message)).await?;
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
        enter_machine_state(session, sink, &config.room_id)?;
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
        file_transfer_version: connected.peer.file_transfer_version.clone(),
        pipe_stream_version: connected.peer.pipe_stream_version,
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
    config: &SessionConfig,
    input: Option<JoinStdinInput>,
    join_state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    authority_send: &mut SendStream,
    transport: &IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> Result<bool, CliError> {
    match input {
        Some(JoinStdinInput::LocalCommand(command)) => {
            if matches!(command, MachineCommand::LeaveRoom) {
                if join_state.self_member.is_some() {
                    send_peer_command(authority_send, &command).await?;
                }
                return Ok(true);
            }
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
                &config.download_dir,
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
            maybe_start_join_pending_receive(
                join_state,
                &config.download_dir,
                transport,
                peer_tx.clone(),
                &event,
            )?;
            emit_event(stdout, &event).await?;
            if let Some(channel_id) = finalized_channel_id(&event) {
                maybe_finalize_join_received_file(join_state, stdout, &channel_id).await?;
            }
            Ok(matches!(event, MachineEvent::RoomClosed { .. }))
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
        None => {
            let event = MachineEvent::RoomClosed {
                reason: "host left".into(),
            };
            apply_machine_event(join_state, &event);
            emit_event(stdout, &event).await?;
            Ok(true)
        }
    }
}

async fn handle_join_peer_input(
    sink: &EventSink,
    transport: &IrohTransport,
    download_dir: &Path,
    join_state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    authority_send: &mut SendStream,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
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
            maybe_start_join_pending_receive(
                join_state,
                download_dir,
                transport,
                peer_tx.clone(),
                &event,
            )?;
            emit_event(stdout, &event).await?;
            if let Some(channel_id) = finalized_channel_id(&event) {
                maybe_finalize_join_received_file(join_state, stdout, &channel_id).await?;
            }
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
            if let Some(channel_id) = finalized_channel_id(&event) {
                maybe_finalize_join_received_file(join_state, stdout, &channel_id).await?;
            }
        }
        Some(JoinPeerInput::PreparedTransferReady {
            channel_id,
            size,
            transfer,
        }) => {
            if !join_state.channels.contains_key(&channel_id) {
                return Ok(());
            }
            send_peer_command(
                authority_send,
                &MachineCommand::UpdateChannelTransfer {
                    channel_id: channel_id.clone(),
                    size: Some(size),
                    transfer: transfer.clone(),
                },
            )
            .await?;
            let local_event = match transfer.item_kind {
                TransferItemKind::File => MachineEvent::ChannelTransferFinalized {
                    channel_id,
                    size: Some(size),
                    transfer,
                },
                TransferItemKind::Folder => MachineEvent::ChannelTransferReady {
                    channel_id,
                    size: Some(size),
                    transfer,
                },
            };
            apply_machine_event(join_state, &local_event);
        }
        Some(JoinPeerInput::PreparedTransferFailed {
            channel_id,
            message,
        }) => {
            if join_state.channels.contains_key(&channel_id) {
                let _ = send_peer_command(
                    authority_send,
                    &MachineCommand::CloseChannel {
                        channel_id: channel_id.clone(),
                        reason: Some("sender failed to prepare transfer".into()),
                    },
                )
                .await;
                join_state.channels.remove(&channel_id);
            }
            emit_event(stdout, &local_error("transfer_prepare_failed", &message)).await?;
        }
        Some(JoinPeerInput::ReceivedFileReadyToFinalize {
            channel_id,
            received,
        }) => {
            join_state.pending_received_files.insert(
                channel_id.clone(),
                PendingReceivedFile {
                    temp_path: received.temp_path,
                    final_path: received.final_path,
                    actual_hash: received.content_hash,
                    size: received.size,
                },
            );
            maybe_finalize_join_received_file(join_state, stdout, &channel_id).await?;
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
    download_dir: &Path,
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
        MachineCommand::LeaveRoom => Ok(()),
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
        MachineCommand::SendPath {
            channel_id,
            to,
            path,
            name,
            mime,
        } => {
            ensure_file_transfer_supported(
                state.host_file_transfer_version,
                "host",
                "remote host",
            )?;
            send_path_from_join(
                state,
                authority_send,
                transport,
                peer_tx,
                self_member,
                channel_id,
                to,
                path,
                name,
                mime,
            )
            .await
        }
        MachineCommand::SendFile {
            channel_id,
            to,
            path,
            name,
            mime,
        } => {
            send_path_from_join(
                state,
                authority_send,
                transport,
                peer_tx,
                self_member,
                channel_id,
                to,
                path,
                name,
                mime,
            )
            .await
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
            transfer,
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
                transfer: transfer.clone(),
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
                    transfer,
                },
            )
            .await?;
            apply_machine_event(state, &local_event);
            Ok(())
        }
        MachineCommand::UpdateChannelTransfer {
            channel_id,
            size,
            transfer,
        } => {
            let local_event = match transfer.item_kind {
                TransferItemKind::File => MachineEvent::ChannelTransferFinalized {
                    channel_id: channel_id.clone(),
                    size,
                    transfer: transfer.clone(),
                },
                TransferItemKind::Folder => MachineEvent::ChannelTransferReady {
                    channel_id: channel_id.clone(),
                    size,
                    transfer: transfer.clone(),
                },
            };
            send_peer_command(
                authority_send,
                &MachineCommand::UpdateChannelTransfer {
                    channel_id,
                    size,
                    transfer,
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
                provider_ticket: _,
                transfer,
                name: _,
                size,
            }) =
                join_pending_file_transfer(state, &self_member, &channel_id)?
            {
                state.pending_accepted_files.insert(channel_id.clone());
                maybe_start_join_pending_receive(
                    state,
                    download_dir,
                    transport,
                    peer_tx,
                    &MachineEvent::ChannelTransferReady {
                        channel_id,
                        size,
                        transfer,
                    },
                )?;
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
        MachineCommand::ExportChannelFile { .. } => Err(CliError::usage(
            "export_channel_file is no longer supported; accepted transfers are saved automatically",
        )),
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

#[allow(clippy::too_many_arguments)]
async fn send_path_from_join(
    state: &mut JoinState,
    authority_send: &mut SendStream,
    transport: &IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    self_member: MemberId,
    channel_id: ChannelId,
    to: skyffla_protocol::room::Route,
    path: String,
    name: Option<String>,
    mime: Option<String>,
) -> Result<(), CliError> {
    let resolved = resolve_send_path(&path)?;
    let display_name = name.unwrap_or_else(|| resolved.display_name.clone());
    let channel_name = Some(display_name.clone());
    if matches!(resolved.item_kind, TransferItemKind::File) {
        let size = resolved.provisional_size.ok_or_else(|| {
            CliError::runtime("single-file transfer is missing a provisional size")
        })?;
        transport
            .register_outgoing_file(
                channel_id.as_str().to_string(),
                &resolved.expanded_path,
                size,
                display_name.clone(),
                None,
                Some(spawn_join_transfer_progress_forwarder(peer_tx.clone())),
            )
            .await;
    }
    let provisional_transfer = provisional_transfer_offer(resolved.item_kind.clone());
    let local_event = MachineEvent::ChannelOpened {
        channel_id: channel_id.clone(),
        kind: ChannelKind::File,
        from: self_member,
        from_name: state.local_name.clone(),
        to: to.clone(),
        name: channel_name.clone(),
        size: resolved.provisional_size,
        mime: mime.clone(),
        transfer: Some(provisional_transfer.clone()),
    };
    if let Err(error) = send_peer_command(
        authority_send,
        &MachineCommand::OpenChannel {
            channel_id: channel_id.clone(),
            kind: ChannelKind::File,
            to,
            name: channel_name,
            size: resolved.provisional_size,
            mime,
            transfer: Some(provisional_transfer),
        },
    )
    .await
    {
        if matches!(resolved.item_kind, TransferItemKind::File) {
            transport
                .unregister_outgoing_transfer(channel_id.as_str())
                .await;
        }
        return Err(error);
    }
    apply_machine_event(state, &local_event);
    spawn_join_prepare_path_task(
        transport.clone(),
        peer_tx,
        channel_id,
        resolved.expanded_path,
        resolved.item_kind,
        display_name,
    );
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

fn resolve_download_dir(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn sanitized_receive_name(name: &str) -> Result<String, CliError> {
    let path = Path::new(name);
    let file_name = path
        .file_name()
        .ok_or_else(|| CliError::usage(format!("transfer item name {:?} is invalid", name)))?;
    let file_name = file_name.to_string_lossy().trim().to_string();
    if file_name.is_empty() || file_name == "." || file_name == ".." {
        return Err(CliError::usage(format!(
            "transfer item name {:?} is invalid",
            name
        )));
    }
    Ok(file_name)
}

fn receive_target_path(download_dir: &Path, name: &str) -> Result<PathBuf, CliError> {
    let root = resolve_download_dir(download_dir);
    std::fs::create_dir_all(&root).map_err(|error| {
        CliError::local_io(format!("failed to create {}: {error}", root.display()))
    })?;
    Ok(unique_path_in_dir(&root, &sanitized_receive_name(name)?))
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

fn ensure_file_transfer_supported(
    version: Option<ProtocolVersion>,
    peer_name: &str,
    side_label: &str,
) -> Result<(), CliError> {
    match version {
        Some(version) if version.is_compatible_with(FILE_TRANSFER_PROTOCOL_VERSION) => Ok(()),
        Some(version) => Err(CliError::usage(format!(
            "cannot use send_path with {peer_name}: local file transfer protocol is {}, {side_label} is {}; update the older Skyffla peer",
            FILE_TRANSFER_PROTOCOL_VERSION, version
        ))),
        None => Err(CliError::usage(format!(
            "cannot use send_path with {peer_name}: {side_label} does not advertise file transfer protocol support; update the older Skyffla peer"
        ))),
    }
}

fn ensure_route_supports_file_transfer(
    host_member: &MemberId,
    peers: &BTreeMap<MemberId, PeerHandle>,
    route: &skyffla_protocol::room::Route,
) -> Result<(), CliError> {
    match route {
        skyffla_protocol::room::Route::All => {
            for peer in peers.values() {
                ensure_file_transfer_supported(
                    peer.file_transfer_version,
                    &peer.member.name,
                    "remote peer",
                )?;
            }
            Ok(())
        }
        skyffla_protocol::room::Route::Member { member_id } => {
            if member_id == host_member {
                return Ok(());
            }
            let peer = peers.get(member_id).ok_or_else(|| {
                CliError::runtime(format!("unknown member {}", member_id.as_str()))
            })?;
            ensure_file_transfer_supported(
                peer.file_transfer_version,
                &peer.member.name,
                "remote peer",
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_path_from_host(
    room: &mut RoomEngine,
    sender: &MemberId,
    transport: &IrohTransport,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    channel_id: ChannelId,
    to: skyffla_protocol::room::Route,
    path: String,
    name: Option<String>,
    mime: Option<String>,
) -> Result<(), CliError> {
    let resolved = resolve_send_path(&path)?;
    let display_name = name.unwrap_or_else(|| resolved.display_name.clone());
    let open_name = Some(display_name.clone());
    if matches!(resolved.item_kind, TransferItemKind::File) {
        let size = resolved.provisional_size.ok_or_else(|| {
            CliError::runtime("single-file transfer is missing a provisional size")
        })?;
        transport
            .register_outgoing_file(
                channel_id.as_str().to_string(),
                &resolved.expanded_path,
                size,
                display_name.clone(),
                None,
                Some(spawn_host_transfer_progress_forwarder(host_tx.clone())),
            )
            .await;
    }
    let provisional_transfer = provisional_transfer_offer(resolved.item_kind.clone());
    let routed = match room
        .open_channel(
            sender,
            channel_id.clone(),
            ChannelKind::File,
            to,
            open_name,
            resolved.provisional_size,
            mime,
            Some(provisional_transfer),
        )
        .map_err(room_error)
    {
        Ok(routed) => routed,
        Err(error) => {
            if matches!(resolved.item_kind, TransferItemKind::File) {
                transport
                    .unregister_outgoing_transfer(channel_id.as_str())
                    .await;
            }
            return Err(error);
        }
    };
    if let Err(error) = deliver_routed_events(room.host_member(), routed, stdout, peers).await {
        if matches!(resolved.item_kind, TransferItemKind::File) {
            transport
                .unregister_outgoing_transfer(channel_id.as_str())
                .await;
        }
        return Err(error);
    }
    spawn_host_prepare_path_task(
        transport.clone(),
        host_tx.clone(),
        channel_id,
        resolved.expanded_path,
        resolved.item_kind,
        display_name,
    );
    Ok(())
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

fn file_transfer_offer(content_hash: String) -> TransferOffer {
    TransferOffer {
        item_kind: TransferItemKind::File,
        integrity: Some(TransferDigest {
            algorithm: "blake3".into(),
            value: content_hash,
        }),
    }
}

fn provisional_transfer_offer(item_kind: TransferItemKind) -> TransferOffer {
    TransferOffer {
        item_kind,
        integrity: None,
    }
}

fn resolve_send_path(path: &str) -> Result<ResolvedSendPath, CliError> {
    let expanded_path = expand_user_path(path);
    let display_name = expanded_path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| expanded_path.display().to_string());
    if expanded_path.is_file() {
        let file_size = std::fs::metadata(&expanded_path)
            .map_err(|error| {
                CliError::local_io(format!(
                    "failed to read metadata for {}: {error}",
                    expanded_path.display()
                ))
            })?
            .len();
        Ok(ResolvedSendPath {
            expanded_path,
            item_kind: TransferItemKind::File,
            display_name,
            provisional_size: Some(file_size),
        })
    } else if expanded_path.is_dir() {
        Ok(ResolvedSendPath {
            expanded_path,
            item_kind: TransferItemKind::Folder,
            display_name,
            provisional_size: None,
        })
    } else {
        Err(CliError::usage(format!(
            "path {} is not a file or directory",
            expanded_path.display()
        )))
    }
}

fn folder_transfer_offer() -> TransferOffer {
    TransferOffer {
        item_kind: TransferItemKind::Folder,
        integrity: None,
    }
}

fn finalized_channel_id(event: &MachineEvent) -> Option<ChannelId> {
    match event {
        MachineEvent::ChannelTransferFinalized { channel_id, .. } => Some(channel_id.clone()),
        _ => None,
    }
}

async fn maybe_finalize_join_received_file(
    state: &mut JoinState,
    stdout: &mut tokio::io::Stdout,
    channel_id: &ChannelId,
) -> Result<(), CliError> {
    let Some(expected_hash) = state
        .channels
        .get(channel_id)
        .and_then(|channel| channel.transfer.as_ref())
        .and_then(|transfer| transfer.integrity.as_ref())
        .map(|integrity| integrity.value.clone())
    else {
        return Ok(());
    };
    let Some(pending) = state.pending_received_files.remove(channel_id) else {
        return Ok(());
    };
    if pending.actual_hash != expected_hash {
        let _ = tokio::fs::remove_file(&pending.temp_path).await;
        emit_event(
            stdout,
            &MachineEvent::Error {
                code: "transfer_integrity_mismatch".into(),
                message: format!(
                    "file transfer verification failed for {}: expected {}, received {}",
                    channel_id.as_str(),
                    expected_hash,
                    pending.actual_hash
                ),
                channel_id: Some(channel_id.clone()),
            },
        )
        .await?;
        return Ok(());
    }
    tokio::fs::rename(&pending.temp_path, &pending.final_path)
        .await
        .map_err(|error| {
            CliError::local_io(format!(
                "failed to finalize {}: {error}",
                pending.final_path.display()
            ))
        })?;
    let event = MachineEvent::ChannelPathReceived {
        channel_id: channel_id.clone(),
        path: pending.final_path.display().to_string(),
        size: pending.size,
    };
    apply_machine_event(state, &event);
    emit_event(stdout, &event).await
}

async fn maybe_finalize_host_received_file(
    room: &RoomEngine,
    host_state: &mut HostState,
    stdout: &mut tokio::io::Stdout,
    channel_id: &ChannelId,
) -> Result<(), CliError> {
    let Some(expected_hash) = room
        .channel(channel_id)
        .and_then(|channel| channel.transfer)
        .and_then(|transfer| transfer.integrity)
        .map(|integrity| integrity.value)
    else {
        return Ok(());
    };
    let Some(pending) = host_state.pending_received_files.remove(channel_id) else {
        return Ok(());
    };
    if pending.actual_hash != expected_hash {
        let _ = tokio::fs::remove_file(&pending.temp_path).await;
        emit_event(
            stdout,
            &MachineEvent::Error {
                code: "transfer_integrity_mismatch".into(),
                message: format!(
                    "file transfer verification failed for {}: expected {}, received {}",
                    channel_id.as_str(),
                    expected_hash,
                    pending.actual_hash
                ),
                channel_id: Some(channel_id.clone()),
            },
        )
        .await?;
        return Ok(());
    }
    tokio::fs::rename(&pending.temp_path, &pending.final_path)
        .await
        .map_err(|error| {
            CliError::local_io(format!(
                "failed to finalize {}: {error}",
                pending.final_path.display()
            ))
        })?;
    let event = MachineEvent::ChannelPathReceived {
        channel_id: channel_id.clone(),
        path: pending.final_path.display().to_string(),
        size: pending.size,
    };
    apply_host_event(host_state, &event);
    emit_event(stdout, &event).await
}

fn maybe_start_join_pending_receive(
    state: &mut JoinState,
    download_dir: &Path,
    transport: &IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    event: &MachineEvent,
) -> Result<(), CliError> {
    let (MachineEvent::ChannelTransferReady { channel_id, .. }
    | MachineEvent::ChannelTransferFinalized { channel_id, .. }) = event
    else {
        return Ok(());
    };
    if !state.pending_accepted_files.contains(channel_id) {
        return Ok(());
    }
    let Some(self_member) = state.self_member.clone() else {
        return Ok(());
    };
    let Some(PendingFileTransfer {
        provider_ticket,
        transfer,
        name,
        size,
    }) = join_pending_file_transfer(state, &self_member, channel_id)?
    else {
        return Ok(());
    };
    match transfer.item_kind.clone() {
        TransferItemKind::File => {
            let Some(size) = size else {
                return Ok(());
            };
            let target_path = receive_target_path(download_dir, &name)?;
            state.pending_accepted_files.remove(channel_id);
            spawn_join_direct_receive(
                transport.clone(),
                peer_tx,
                PeerTicket {
                    encoded: provider_ticket,
                },
                channel_id.clone(),
                name,
                size,
                target_path,
            );
        }
        TransferItemKind::Folder => {
            let target_path = receive_target_path(download_dir, &name)?;
            state.pending_accepted_files.remove(channel_id);
            spawn_join_direct_directory_receive(
                transport.clone(),
                peer_tx,
                PeerTicket {
                    encoded: provider_ticket,
                },
                channel_id.clone(),
                name,
                size,
                target_path,
            );
        }
    }
    Ok(())
}

fn host_pending_file_download(
    room: &RoomEngine,
    peers: &BTreeMap<MemberId, PeerHandle>,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<Option<(PeerTicket, FileTransferTarget)>, CliError> {
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
    let transfer = channel.transfer.ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing transfer metadata",
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
    Ok(Some((
        PeerTicket { encoded: ticket },
        FileTransferTarget {
            transfer,
            name: channel
                .name
                .unwrap_or_else(|| channel_id.as_str().to_string()),
            size: channel.size,
        },
    )))
}

fn maybe_start_host_pending_receive(
    room: &RoomEngine,
    peers: &BTreeMap<MemberId, PeerHandle>,
    host_state: &mut HostState,
    self_member: &MemberId,
    channel_id: &ChannelId,
    download_dir: &Path,
    transport: &IrohTransport,
    host_tx: &mpsc::UnboundedSender<HostInput>,
) -> Result<(), CliError> {
    if !host_state.pending_accepted_files.contains(channel_id) {
        return Ok(());
    }
    let Some((provider, file)) = host_pending_file_download(room, peers, self_member, channel_id)?
    else {
        return Ok(());
    };
    match file.transfer.item_kind.clone() {
        TransferItemKind::File => {
            let Some(size) = file.size else {
                return Ok(());
            };
            let target_path = receive_target_path(download_dir, &file.name)?;
            host_state.pending_accepted_files.remove(channel_id);
            spawn_host_direct_receive(
                transport.clone(),
                host_tx.clone(),
                provider,
                channel_id.clone(),
                file.name,
                size,
                target_path,
            );
        }
        TransferItemKind::Folder => {
            let target_path = receive_target_path(download_dir, &file.name)?;
            host_state.pending_accepted_files.remove(channel_id);
            spawn_host_direct_directory_receive(
                transport.clone(),
                host_tx.clone(),
                provider,
                channel_id.clone(),
                file.name,
                file.size,
                target_path,
            );
        }
    }
    Ok(())
}

async fn handle_host_command(
    room: &mut RoomEngine,
    sender: &MemberId,
    command: MachineCommand,
    download_dir: &Path,
    stdout: &mut tokio::io::Stdout,
    peers: &mut BTreeMap<MemberId, PeerHandle>,
    host_state: &mut HostState,
    transport: &IrohTransport,
    host_tx: &mpsc::UnboundedSender<HostInput>,
    local_command: bool,
) -> Result<(), CliError> {
    command.validate().map_err(protocol_error)?;

    match command {
        MachineCommand::LeaveRoom => {
            if !local_command {
                return Err(CliError::usage(
                    "leave_room is only valid as a local machine command",
                ));
            }
            broadcast_room_closed(
                peers,
                MachineEvent::RoomClosed {
                    reason: "host left".into(),
                },
            )
        }
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
            transfer,
        } => {
            let routed = room
                .open_channel(sender, channel_id, kind, to, name, size, mime, transfer)
                .map_err(room_error)?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await
        }
        MachineCommand::UpdateChannelTransfer {
            channel_id,
            size,
            transfer,
        } => {
            let routed = room
                .update_channel_transfer(sender, &channel_id, size, transfer)
                .map_err(room_error)?;
            maybe_start_host_pending_receive(
                room,
                peers,
                host_state,
                room.host_member(),
                &channel_id,
                download_dir,
                transport,
                host_tx,
            )?;
            deliver_routed_events(room.host_member(), routed, stdout, peers).await?;
            maybe_finalize_host_received_file(room, host_state, stdout, &channel_id).await?;
            Ok(())
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
        MachineCommand::SendPath {
            channel_id,
            to,
            path,
            name,
            mime,
        } => {
            ensure_route_supports_file_transfer(room.host_member(), peers, &to)?;
            send_path_from_host(
                room,
                sender,
                transport,
                host_tx,
                stdout,
                peers,
                channel_id,
                to,
                path,
                name,
                mime,
            )
            .await
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
            send_path_from_host(
                room,
                sender,
                transport,
                host_tx,
                stdout,
                peers,
                channel_id,
                to,
                path,
                name,
                mime,
            )
            .await
        }
        MachineCommand::ExportChannelFile { .. } => Err(CliError::usage(
            "export_channel_file is no longer supported; accepted transfers are saved automatically",
        )),
        MachineCommand::AcceptChannel { channel_id } => {
            let routed = room
                .accept_channel(sender, &channel_id)
                .map_err(room_error)?;
            if local_command {
                if let Some((_provider, _file)) =
                    host_pending_file_download(room, peers, sender, &channel_id)?
                {
                    host_state.pending_accepted_files.insert(channel_id.clone());
                    maybe_start_host_pending_receive(
                        room,
                        peers,
                        host_state,
                        sender,
                        &channel_id,
                        download_dir,
                        transport,
                        host_tx,
                    )?;
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
        MachineCommand::LeaveRoom => None,
        MachineCommand::SendChat { .. } => None,
        MachineCommand::SendFile { channel_id, .. }
        | MachineCommand::SendPath { channel_id, .. }
        | MachineCommand::OpenChannel { channel_id, .. }
        | MachineCommand::UpdateChannelTransfer { channel_id, .. }
        | MachineCommand::AcceptChannel { channel_id }
        | MachineCommand::RejectChannel { channel_id, .. }
        | MachineCommand::SendChannelData { channel_id, .. }
        | MachineCommand::CloseChannel { channel_id, .. }
        | MachineCommand::ExportChannelFile { channel_id, .. } => Some(channel_id.clone()),
    }
}

fn broadcast_room_closed(
    peers: &BTreeMap<MemberId, PeerHandle>,
    event: MachineEvent,
) -> Result<(), CliError> {
    for peer in peers.values() {
        send_link_message(
            &peer.authority_sender,
            AuthorityLinkMessage::MachineEvent {
                event: event.clone(),
            },
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use skyffla_protocol::room::{ChannelKind, Route, TransferDigest};
    use skyffla_session::room::RoomEngine;

    use super::*;
    use crate::runtime::machine_state::JoinChannelState;

    fn join_state_with_pending_file(
        self_member: &MemberId,
        host_member: &MemberId,
        channel_id: &ChannelId,
        transfer: TransferOffer,
    ) -> JoinState {
        let mut channels = BTreeMap::new();
        channels.insert(
            channel_id.clone(),
            JoinChannelState {
                opener: host_member.clone(),
                kind: ChannelKind::File,
                participants: BTreeSet::from([host_member.clone(), self_member.clone()]),
                name: Some("report.txt".into()),
                size: Some(11),
                transfer: Some(transfer),
                local_file_ready: false,
            },
        );
        let mut member_tickets = BTreeMap::new();
        member_tickets.insert(host_member.clone(), "provider-ticket".into());

        JoinState {
            self_member: Some(self_member.clone()),
            host_member: Some(host_member.clone()),
            members: BTreeMap::new(),
            member_tickets,
            channels,
            peer_links: BTreeMap::new(),
            pending_accepted_files: BTreeSet::from([channel_id.clone()]),
            pending_received_files: BTreeMap::new(),
            local_name: "beta".into(),
            local_fingerprint: None,
            local_ticket: "local-ticket".into(),
            pending_host_ticket: None,
            host_file_transfer_version: None,
        }
    }

    fn host_room_with_pending_file(
        channel_id: &ChannelId,
        transfer: TransferOffer,
    ) -> (
        RoomEngine,
        HostState,
        BTreeMap<MemberId, PeerHandle>,
        MemberId,
    ) {
        let mut room = RoomEngine::new(RoomId::new("demo-room").unwrap(), "alpha", None).unwrap();
        let beta = room.join("beta", None).unwrap();
        let host_member = room.host_member().clone();
        room.open_channel(
            &beta.member.member_id,
            channel_id.clone(),
            ChannelKind::File,
            Route::Member {
                member_id: host_member.clone(),
            },
            Some("report.txt".into()),
            Some(11),
            None,
            Some(transfer),
        )
        .unwrap();

        let (authority_sender, _authority_rx) = mpsc::unbounded_channel();
        let mut peers = BTreeMap::new();
        peers.insert(
            beta.member.member_id.clone(),
            PeerHandle {
                authority_sender,
                peer_sender: None,
                member: beta.member,
                ticket: Some("provider-ticket".into()),
                file_transfer_version: None,
                pipe_stream_version: None,
                pending_events: Vec::new(),
            },
        );

        (
            room,
            HostState {
                pending_accepted_files: BTreeSet::from([channel_id.clone()]),
                pending_received_files: BTreeMap::new(),
            },
            peers,
            host_member,
        )
    }

    #[tokio::test]
    async fn provisional_file_accept_starts_receive_before_integrity_is_available() {
        let host_member = MemberId::new("m1").unwrap();
        let self_member = MemberId::new("m2").unwrap();
        let channel_id = ChannelId::new("file-1").unwrap();
        let transfer = TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: None,
        };
        let mut state =
            join_state_with_pending_file(&self_member, &host_member, &channel_id, transfer.clone());
        let transport = IrohTransport::bind().await.unwrap();
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();

        maybe_start_join_pending_receive(
            &mut state,
            Path::new("."),
            &transport,
            peer_tx,
            &MachineEvent::ChannelTransferReady {
                channel_id: channel_id.clone(),
                size: Some(11),
                transfer,
            },
        )
        .unwrap();

        assert!(!state.pending_accepted_files.contains(&channel_id));
        assert!(state.pending_received_files.is_empty());
        let _ = peer_rx.try_recv();

        transport.close().await;
    }

    #[tokio::test]
    async fn provisional_host_file_accept_starts_receive_before_integrity_is_available() {
        let channel_id = ChannelId::new("file-1").unwrap();
        let transfer = TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: None,
        };
        let (room, mut host_state, peers, host_member) =
            host_room_with_pending_file(&channel_id, transfer);
        let transport = IrohTransport::bind().await.unwrap();
        let (host_tx, mut host_rx) = mpsc::unbounded_channel();

        maybe_start_host_pending_receive(
            &room,
            &peers,
            &mut host_state,
            &host_member,
            &channel_id,
            Path::new("."),
            &transport,
            &host_tx,
        )
        .unwrap();

        assert!(!host_state.pending_accepted_files.contains(&channel_id));
        assert!(host_state.pending_received_files.is_empty());
        let _ = host_rx.try_recv();

        transport.close().await;
    }

    #[tokio::test]
    async fn finalized_file_accept_starts_receive_and_clears_pending_flag() {
        let host_member = MemberId::new("m1").unwrap();
        let self_member = MemberId::new("m2").unwrap();
        let channel_id = ChannelId::new("file-1").unwrap();
        let transfer = TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: Some(TransferDigest {
                algorithm: "blake3".into(),
                value: "feedbeef".into(),
            }),
        };
        let mut state =
            join_state_with_pending_file(&self_member, &host_member, &channel_id, transfer.clone());
        let transport = IrohTransport::bind().await.unwrap();
        let (peer_tx, _peer_rx) = mpsc::unbounded_channel();

        maybe_start_join_pending_receive(
            &mut state,
            Path::new("."),
            &transport,
            peer_tx,
            &MachineEvent::ChannelTransferFinalized {
                channel_id: channel_id.clone(),
                size: Some(11),
                transfer,
            },
        )
        .unwrap();

        assert!(!state.pending_accepted_files.contains(&channel_id));

        transport.close().await;
    }

    #[tokio::test]
    async fn finalized_host_file_accept_starts_receive_and_clears_pending_flag() {
        let channel_id = ChannelId::new("file-1").unwrap();
        let transfer = TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: Some(TransferDigest {
                algorithm: "blake3".into(),
                value: "feedbeef".into(),
            }),
        };
        let (mut room, mut host_state, peers, host_member) = host_room_with_pending_file(
            &channel_id,
            provisional_transfer_offer(TransferItemKind::File),
        );
        let beta_member = MemberId::new("m2").unwrap();
        room.update_channel_transfer(&beta_member, &channel_id, Some(11), transfer)
            .unwrap();
        let transport = IrohTransport::bind().await.unwrap();
        let (host_tx, _host_rx) = mpsc::unbounded_channel();

        maybe_start_host_pending_receive(
            &room,
            &peers,
            &mut host_state,
            &host_member,
            &channel_id,
            Path::new("."),
            &transport,
            &host_tx,
        )
        .unwrap();

        assert!(!host_state.pending_accepted_files.contains(&channel_id));

        transport.close().await;
    }
}
