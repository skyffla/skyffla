use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use skyffla_session::{state_changed_event, SessionEvent, SessionMachine};
use skyffla_transport::{IrohTransport, PeerTicket};

use crate::app::sink::EventSink;
use crate::config::{Role, SessionArgs, SessionConfig};
use crate::net::rendezvous::{delete_stream, register_stream, resolve_stream};
use crate::runtime::session::run_connected_session;

pub(crate) async fn run_host(args: SessionArgs) -> Result<()> {
    let config = SessionConfig::from_args(Role::Host, args);
    let sink = EventSink::from_config(&config);
    let client = Client::new();
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::HostRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter hosting state")?,
    ));

    let transport = IrohTransport::bind().await?;
    let ticket = transport.local_ticket()?;
    register_stream(&client, &config, &ticket).await?;
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")?,
    ));

    if config.json_events {
        sink.emit_json_event(json!({
            "event": "waiting",
            "stream_id": config.stream_id,
            "server": config.rendezvous_server,
        }));
    } else {
        eprintln!(
            "waiting for peer on stream {} via {}",
            config.stream_id, config.rendezvous_server
        );
    }
    let connection = transport.accept_connection().await?;
    let result =
        run_connected_session(&config, &sink, &mut session, &transport, connection, true).await;
    let delete_result = delete_stream(&client, &config).await;
    transport.close().await;
    delete_result?;
    result
}

pub(crate) async fn run_join(args: SessionArgs) -> Result<()> {
    let config = SessionConfig::from_args(Role::Join, args);
    let sink = EventSink::from_config(&config);
    let client = Client::new();
    let mut session = SessionMachine::new();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::JoinRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter joining state")?,
    ));

    let transport = IrohTransport::bind().await?;
    let peer = resolve_stream(&client, &config).await?;
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")?,
    ));
    let connection = transport
        .connect(&PeerTicket {
            encoded: peer.ticket,
        })
        .await?;
    let result =
        run_connected_session(&config, &sink, &mut session, &transport, connection, false).await;
    transport.close().await;
    result
}
