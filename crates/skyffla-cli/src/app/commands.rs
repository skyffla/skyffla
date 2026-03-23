use anyhow::Context;
use iroh::EndpointAddr;
use reqwest::Client;
use serde_json::json;
use skyffla_rendezvous::GetRoomResponse;
use skyffla_session::{state_changed_event, SessionEvent, SessionMachine};
use skyffla_transport::{IrohTransport, PeerTicket, TransportOptions};

use crate::app::sink::EventSink;
use crate::cli_error::CliError;
use crate::config::{Role, SessionArgs, SessionConfig};
use crate::net::local_discovery::{
    enable_local_discovery, resolve_local_join_decision, LocalAnnouncement, LocalJoinDecision,
};
use crate::net::rendezvous::{delete_room, register_room, resolve_room, RegisterRoomError};
use crate::runtime::machine::run_machine_host;
use crate::runtime::room_tui::run_room_tui;
use crate::runtime::session::run_connected_session;

pub(crate) async fn run_host(args: SessionArgs) -> Result<(), CliError> {
    let config = SessionConfig::from_args(Role::Host, args)?;
    if should_use_room_tui(&config) {
        return run_room_tui(Role::Host, &config).await;
    }
    let sink = EventSink::from_config(&config);
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::HostRequested {
                room_id: config.room_id.clone(),
            })
            .context("failed to enter hosting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    if config.local_mode {
        return run_host_local(&config, &sink, &mut session).await;
    }

    let client = Client::new();
    let transport = IrohTransport::bind_with_options(TransportOptions { local_only: false })
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    register_room(&client, &config, &ticket)
        .await
        .map_err(|error| match error {
            RegisterRoomError::AlreadyHosted { room_id, .. } => {
                CliError::rendezvous(format!("room {room_id} is already hosted"))
            }
            RegisterRoomError::Other(error) => CliError::rendezvous(error.to_string()),
        })?;
    wait_for_incoming_peer(&config, &sink, &mut session, &client, transport, false).await
}

pub(crate) async fn run_join(args: SessionArgs) -> Result<(), CliError> {
    let mut config = SessionConfig::from_args(Role::Join, args)?;
    if should_use_room_tui(&config) {
        return run_room_tui(Role::Join, &config).await;
    }
    let sink = EventSink::from_config(&config);
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::JoinRequested {
                room_id: config.room_id.clone(),
            })
            .context("failed to enter joining state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    if config.local_mode {
        return run_join_local(&mut config, &sink, &mut session).await;
    }

    let client = Client::new();
    let transport = IrohTransport::bind_with_options(TransportOptions { local_only: false })
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let peer = resolve_room(&client, &config)
        .await
        .map_err(|error| CliError::rendezvous(error.to_string()))?;
    if let Some(peer) = peer {
        return connect_to_registered_peer(&config, &sink, &mut session, transport, peer).await;
    }

    let ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    match register_room(&client, &config, &ticket).await {
        Ok(()) => {
            config.role = Role::Host;
            sink.emit_runtime_event(state_changed_event(
                session
                    .transition(SessionEvent::HostRequested {
                        room_id: config.room_id.clone(),
                    })
                    .context("failed to enter hosting state after join lookup")
                    .map_err(|error| CliError::runtime(error.to_string()))?,
            ));
            wait_for_incoming_peer(&config, &sink, &mut session, &client, transport, true).await
        }
        Err(RegisterRoomError::AlreadyHosted { room_id, .. }) => {
            let peer = resolve_room(&client, &config)
                .await
                .map_err(|error| CliError::rendezvous(error.to_string()))?
                .ok_or_else(|| {
                    CliError::rendezvous(format!(
                        "room {room_id} became hosted while joining; retry"
                    ))
                })?;
            connect_to_registered_peer(&config, &sink, &mut session, transport, peer).await
        }
        Err(RegisterRoomError::Other(error)) => Err(CliError::rendezvous(error.to_string())),
    }
}

fn should_use_room_tui(config: &SessionConfig) -> bool {
    !config.machine
}

async fn connect_to_registered_peer(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: IrohTransport,
    peer: GetRoomResponse,
) -> Result<(), CliError> {
    connect_to_endpoint_addr(
        config,
        sink,
        session,
        transport,
        PeerTicket {
            encoded: peer.ticket,
        }
        .to_endpoint_addr()
        .map_err(|error| CliError::transport(error.to_string()))?,
    )
    .await
}

async fn connect_to_endpoint_addr(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: IrohTransport,
    endpoint_addr: EndpointAddr,
) -> Result<(), CliError> {
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    let ticket = PeerTicket::from_endpoint_addr(&endpoint_addr)
        .map_err(|error| CliError::transport(error.to_string()))?;
    let connection = transport
        .connect(&ticket)
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let result = run_connected_session(config, sink, session, &transport, connection, false).await;
    transport.close().await;
    result
}

async fn run_host_local(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
) -> Result<(), CliError> {
    let transport = IrohTransport::bind_with_options(TransportOptions { local_only: true })
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    enable_local_discovery(
        transport.endpoint(),
        &config.room_id,
        LocalAnnouncement::Host,
    )
    .map_err(|error| CliError::transport(error.to_string()))?;
    wait_for_incoming_peer_local(config, sink, session, transport, false).await
}

async fn run_join_local(
    config: &mut SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
) -> Result<(), CliError> {
    let transport = IrohTransport::bind_with_options(TransportOptions { local_only: true })
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let mdns = enable_local_discovery(
        transport.endpoint(),
        &config.room_id,
        LocalAnnouncement::Candidate,
    )
    .map_err(|error| CliError::transport(error.to_string()))?;

    match resolve_local_join_decision(&mdns, &config.room_id, transport.endpoint().id())
        .await
        .map_err(|error| CliError::transport(error.to_string()))?
    {
        LocalJoinDecision::Connect(endpoint_addr) => {
            connect_to_endpoint_addr(config, sink, session, transport, endpoint_addr).await
        }
        LocalJoinDecision::Host => {
            config.role = Role::Host;
            sink.emit_runtime_event(state_changed_event(
                session
                    .transition(SessionEvent::HostRequested {
                        room_id: config.room_id.clone(),
                    })
                    .context("failed to enter hosting state after local discovery")
                    .map_err(|error| CliError::runtime(error.to_string()))?,
            ));
            let _host_mdns = enable_local_discovery(
                transport.endpoint(),
                &config.room_id,
                LocalAnnouncement::Host,
            )
            .map_err(|error| CliError::transport(error.to_string()))?;
            wait_for_incoming_peer_local(config, sink, session, transport, true).await
        }
    }
}

async fn wait_for_incoming_peer(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    client: &Client,
    transport: IrohTransport,
    created_by_join: bool,
) -> Result<(), CliError> {
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    if config.json_events {
        sink.emit_json_event(json!({
            "event": "waiting",
            "room_id": config.room_id,
            "server": config.rendezvous_server,
            "created_by_join": created_by_join,
        }));
    } else if created_by_join {
        eprintln!(
            "no host found for room {}; you are now waiting for a peer via {}",
            config.room_id, config.rendezvous_server
        );
    } else {
        eprintln!(
            "waiting for peer on room {} via {}",
            config.room_id, config.rendezvous_server
        );
    }

    let result = run_machine_host(config, sink, session, &transport).await;
    let delete_result = delete_room(client, config)
        .await
        .map_err(|error| CliError::rendezvous(error.to_string()));
    transport.close().await;
    delete_result?;
    result
}

async fn wait_for_incoming_peer_local(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: IrohTransport,
    created_by_join: bool,
) -> Result<(), CliError> {
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    if config.json_events {
        sink.emit_json_event(json!({
            "event": "waiting",
            "room_id": config.room_id,
            "created_by_join": created_by_join,
            "discovery": "mdns",
        }));
    } else if created_by_join {
        eprintln!(
            "no local host found for room {}; you are now advertising on the local network",
            config.room_id
        );
    } else {
        eprintln!(
            "waiting for local peer on room {} via mDNS discovery",
            config.room_id
        );
    }

    let result = run_machine_host(config, sink, session, &transport).await;
    transport.close().await;
    result
}
