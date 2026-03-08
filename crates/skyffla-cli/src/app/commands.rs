use anyhow::Context;
use reqwest::Client;
use serde_json::json;
use skyffla_rendezvous::GetStreamResponse;
use skyffla_session::{state_changed_event, SessionEvent, SessionMachine};
use skyffla_transport::{IrohTransport, PeerTicket};

use crate::app::sink::EventSink;
use crate::cli_error::CliError;
use crate::config::{Role, SessionArgs, SessionConfig};
use crate::net::rendezvous::{delete_stream, register_stream, resolve_stream, RegisterStreamError};
use crate::runtime::session::run_connected_session;

pub(crate) async fn run_host(args: SessionArgs) -> Result<(), CliError> {
    let config = SessionConfig::from_args(Role::Host, args)?;
    let sink = EventSink::from_config(&config);
    let client = Client::new();
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::HostRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter hosting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    let transport = IrohTransport::bind()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    register_stream(&client, &config, &ticket)
        .await
        .map_err(|error| match error {
            RegisterStreamError::AlreadyHosted { stream_id, .. } => {
                CliError::rendezvous(format!("stream {stream_id} is already hosted"))
            }
            RegisterStreamError::Other(error) => CliError::rendezvous(error.to_string()),
        })?;
    wait_for_incoming_peer(&config, &sink, &mut session, &client, transport, false).await
}

pub(crate) async fn run_join(args: SessionArgs) -> Result<(), CliError> {
    let mut config = SessionConfig::from_args(Role::Join, args)?;
    let sink = EventSink::from_config(&config);
    let client = Client::new();
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::JoinRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter joining state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    let transport = IrohTransport::bind()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let peer = resolve_stream(&client, &config)
        .await
        .map_err(|error| CliError::rendezvous(error.to_string()))?;
    if let Some(peer) = peer {
        return connect_to_registered_peer(&config, &sink, &mut session, transport, peer).await;
    }

    let ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    match register_stream(&client, &config, &ticket).await {
        Ok(()) => {
            config.role = Role::Host;
            sink.emit_runtime_event(state_changed_event(
                session
                    .transition(SessionEvent::HostRequested {
                        stream_id: config.stream_id.clone(),
                    })
                    .context("failed to enter hosting state after join lookup")
                    .map_err(|error| CliError::runtime(error.to_string()))?,
            ));
            wait_for_incoming_peer(&config, &sink, &mut session, &client, transport, true).await
        }
        Err(RegisterStreamError::AlreadyHosted { stream_id, .. }) => {
            let peer = resolve_stream(&client, &config)
                .await
                .map_err(|error| CliError::rendezvous(error.to_string()))?
                .ok_or_else(|| {
                    CliError::rendezvous(format!(
                        "stream {stream_id} became hosted while joining; retry"
                    ))
                })?;
            connect_to_registered_peer(&config, &sink, &mut session, transport, peer).await
        }
        Err(RegisterStreamError::Other(error)) => Err(CliError::rendezvous(error.to_string())),
    }
}

async fn connect_to_registered_peer(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: IrohTransport,
    peer: GetStreamResponse,
) -> Result<(), CliError> {
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    let connection = transport
        .connect(&PeerTicket {
            encoded: peer.ticket,
        })
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let result = run_connected_session(config, sink, session, &transport, connection, false).await;
    transport.close().await;
    result
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
            "stream_id": config.stream_id,
            "server": config.rendezvous_server,
            "created_by_join": created_by_join,
        }));
    } else if created_by_join {
        eprintln!(
            "no host found on stream {}; you are now waiting for a peer via {}",
            config.stream_id, config.rendezvous_server
        );
    } else {
        eprintln!(
            "waiting for peer on stream {} via {}",
            config.stream_id, config.rendezvous_server
        );
    }

    let connection = transport
        .accept_connection()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let result = run_connected_session(config, sink, session, &transport, connection, true).await;
    let delete_result = delete_stream(client, config)
        .await
        .map_err(|error| CliError::rendezvous(error.to_string()));
    transport.close().await;
    delete_result?;
    result
}
