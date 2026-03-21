use std::collections::BTreeMap;
use std::path::PathBuf;

use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::ProtocolVersion;
use skyffla_protocol::room::{
    ChannelId, MachineCommand, MachineEvent, Member, MemberId, TransferItemKind, TransferPhase,
};
use skyffla_protocol::room_link::{AuthorityLinkMessage, PeerLinkMessage};
use skyffla_session::SessionPeer;
use skyffla_transport::{
    ConnectionStatus, IrohConnection, IrohTransport, LocalTransferProgress, PeerTicket,
};
use tokio::sync::mpsc;

use crate::cli_error::CliError;
use crate::config::SessionConfig;
use crate::net::framing::{read_framed, write_framed};
use crate::runtime::handshake::exchange_hello;

#[derive(Debug)]
pub(crate) struct ConnectedPeer {
    pub(crate) peer: SessionPeer,
    pub(crate) connection_status: ConnectionStatus,
    pub(crate) authority_send: SendStream,
    pub(crate) authority_recv: RecvStream,
    pub(crate) connection: IrohConnection,
}

#[derive(Debug)]
pub(crate) struct PeerHandle {
    pub(crate) authority_sender: mpsc::UnboundedSender<AuthorityLinkMessage>,
    pub(crate) peer_sender: Option<mpsc::UnboundedSender<PeerLinkMessage>>,
    pub(crate) member: Member,
    pub(crate) ticket: Option<String>,
    pub(crate) file_transfer_version: Option<ProtocolVersion>,
    pub(crate) pending_events: Vec<MachineEvent>,
}

#[derive(Debug)]
pub(crate) enum HostInput {
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
pub(crate) enum JoinPeerInput {
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

pub(crate) fn spawn_accept_loop(
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

pub(crate) fn spawn_host_transfer_accept_loop(
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let connection = match transport.accept_transfer_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = host_tx.send(HostInput::PeerProtocolError {
                        message: error.to_string(),
                    });
                    break;
                }
            };
            if let Err(error) = transport.serve_registered_transfer(connection).await {
                let _ = host_tx.send(HostInput::PeerProtocolError {
                    message: error.to_string(),
                });
            }
        }
    })
}

pub(crate) fn spawn_join_transfer_accept_loop(
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let connection = match transport.accept_transfer_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                        message: error.to_string(),
                    });
                    break;
                }
            };
            if let Err(error) = transport.serve_registered_transfer(connection).await {
                let _ = peer_tx.send(JoinPeerInput::ProtocolError {
                    message: error.to_string(),
                });
            }
        }
    })
}

pub(crate) fn spawn_host_authority_reader(
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

pub(crate) fn spawn_join_accept_loop(
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

pub(crate) fn spawn_join_host_peer_accept(
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

pub(crate) fn spawn_host_peer_link(
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

pub(crate) fn spawn_outbound_peer_link(
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

pub(crate) fn spawn_join_direct_receive(
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    expected_hash: String,
    name: String,
    size: Option<u64>,
    target_path: PathBuf,
) {
    tokio::spawn(async move {
        let mut last_step = None;
        match transport
            .receive_file_with_progress(
                &provider,
                channel_id.as_str(),
                &expected_hash,
                &target_path,
                |progress| {
                    if should_emit_progress(&mut last_step, &progress, size) {
                        let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                            event: MachineEvent::TransferProgress {
                                channel_id: channel_id.clone(),
                                item_kind: TransferItemKind::File,
                                name: name.clone(),
                                phase: TransferPhase::Downloading,
                                bytes_complete: progress.bytes_complete,
                                bytes_total: size.or(progress.bytes_total),
                            },
                        });
                    }
                },
            )
            .await
        {
            Ok(size) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::ChannelPathReceived {
                        channel_id,
                        path: target_path.display().to_string(),
                        size,
                    },
                });
            }
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "direct_receive_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

pub(crate) fn spawn_host_direct_receive(
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    expected_hash: String,
    name: String,
    size: Option<u64>,
    target_path: PathBuf,
) {
    tokio::spawn(async move {
        let mut last_step = None;
        match transport
            .receive_file_with_progress(
                &provider,
                channel_id.as_str(),
                &expected_hash,
                &target_path,
                |progress| {
                    if should_emit_progress(&mut last_step, &progress, size) {
                        let _ = host_tx.send(HostInput::LocalEvent {
                            event: MachineEvent::TransferProgress {
                                channel_id: channel_id.clone(),
                                item_kind: TransferItemKind::File,
                                name: name.clone(),
                                phase: TransferPhase::Downloading,
                                bytes_complete: progress.bytes_complete,
                                bytes_total: size.or(progress.bytes_total),
                            },
                        });
                    }
                },
            )
            .await
        {
            Ok(size) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::ChannelPathReceived {
                        channel_id,
                        path: target_path.display().to_string(),
                        size,
                    },
                });
            }
            Err(error) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "direct_receive_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

pub(crate) fn spawn_join_direct_directory_receive(
    transport: IrohTransport,
    peer_tx: mpsc::UnboundedSender<JoinPeerInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    transfer_digest: String,
    name: String,
    size: Option<u64>,
    target_path: PathBuf,
) {
    tokio::spawn(async move {
        let mut last_step = None;
        match transport
            .receive_directory_with_progress(&provider, channel_id.as_str(), &transfer_digest, &target_path, |progress| {
                if should_emit_progress(&mut last_step, &progress, size) {
                    let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                        event: MachineEvent::TransferProgress {
                            channel_id: channel_id.clone(),
                            item_kind: TransferItemKind::Folder,
                            name: name.clone(),
                            phase: TransferPhase::Downloading,
                            bytes_complete: progress.bytes_complete,
                            bytes_total: size.or(progress.bytes_total),
                        },
                    });
                }
            })
            .await
        {
            Ok(size) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::TransferProgress {
                        channel_id: channel_id.clone(),
                        item_kind: TransferItemKind::Folder,
                        name: name.clone(),
                        phase: TransferPhase::Exporting,
                        bytes_complete: 0,
                        bytes_total: Some(size),
                    },
                });
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::ChannelPathReceived {
                        channel_id,
                        path: target_path.display().to_string(),
                        size,
                    },
                });
            }
            Err(error) => {
                let _ = peer_tx.send(JoinPeerInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "direct_receive_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

pub(crate) fn spawn_host_direct_directory_receive(
    transport: IrohTransport,
    host_tx: mpsc::UnboundedSender<HostInput>,
    provider: PeerTicket,
    channel_id: ChannelId,
    transfer_digest: String,
    name: String,
    size: Option<u64>,
    target_path: PathBuf,
) {
    tokio::spawn(async move {
        let mut last_step = None;
        match transport
            .receive_directory_with_progress(&provider, channel_id.as_str(), &transfer_digest, &target_path, |progress| {
                if should_emit_progress(&mut last_step, &progress, size) {
                    let _ = host_tx.send(HostInput::LocalEvent {
                        event: MachineEvent::TransferProgress {
                            channel_id: channel_id.clone(),
                            item_kind: TransferItemKind::Folder,
                            name: name.clone(),
                            phase: TransferPhase::Downloading,
                            bytes_complete: progress.bytes_complete,
                            bytes_total: size.or(progress.bytes_total),
                        },
                    });
                }
            })
            .await
        {
            Ok(size) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::TransferProgress {
                        channel_id: channel_id.clone(),
                        item_kind: TransferItemKind::Folder,
                        name: name.clone(),
                        phase: TransferPhase::Exporting,
                        bytes_complete: 0,
                        bytes_total: Some(size),
                    },
                });
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::ChannelPathReceived {
                        channel_id,
                        path: target_path.display().to_string(),
                        size,
                    },
                });
            }
            Err(error) => {
                let _ = host_tx.send(HostInput::LocalEvent {
                    event: MachineEvent::Error {
                        code: "direct_receive_failed".into(),
                        message: error.to_string(),
                        channel_id: Some(channel_id),
                    },
                });
            }
        }
    });
}

fn should_emit_progress(
    last_step: &mut Option<u64>,
    progress: &LocalTransferProgress,
    total_override: Option<u64>,
) -> bool {
    let total = total_override.or(progress.bytes_total);
    let step = match total {
        Some(total) if total > 0 => (progress.bytes_complete.saturating_mul(100) / total) / 10,
        _ => progress.bytes_complete / (256 * 1024),
    };
    let emit = *last_step != Some(step) || progress.bytes_complete == 0;
    *last_step = Some(step);
    emit
}

pub(crate) fn introduce_member_to_existing_peers(
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

pub(crate) async fn read_authority_link_message(
    recv: &mut RecvStream,
) -> Result<Option<AuthorityLinkMessage>, CliError> {
    read_framed::<AuthorityLinkMessage>(recv, "authority link message")
        .await
        .map_err(|error| CliError::protocol(error.to_string()))
}

pub(crate) async fn read_peer_link_message(
    recv: &mut RecvStream,
) -> Result<Option<PeerLinkMessage>, CliError> {
    read_framed::<PeerLinkMessage>(recv, "peer link message")
        .await
        .map_err(|error| CliError::protocol(error.to_string()))
}

pub(crate) fn peer_event_matches_sender(member_id: &MemberId, event: &MachineEvent) -> bool {
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
        | MachineEvent::RoomClosed { .. }
        | MachineEvent::ChannelPathReceived { .. }
        | MachineEvent::TransferProgress { .. }
        | MachineEvent::Error { .. } => true,
    }
}

pub(crate) fn send_link_message<T>(
    sender: &mpsc::UnboundedSender<T>,
    message: T,
) -> Result<(), CliError> {
    sender
        .send(message)
        .map_err(|_| CliError::protocol("room link channel closed"))
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

pub(crate) fn spawn_link_writer<T>(
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

fn spawn_host_peer_reader(
    mut recv: RecvStream,
    member_id: MemberId,
    host_tx: mpsc::UnboundedSender<HostInput>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_peer_link_message(&mut recv).await {
                Ok(Some(PeerLinkMessage::MachineEvent { event })) => {
                    if !peer_event_matches_sender(&member_id, &event) {
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
