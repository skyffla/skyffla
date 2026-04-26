use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_session::{state_changed_event, RuntimeEvent, SessionEvent, SessionMachine};
use skyffla_transport::{IrohConnection, IrohTransport};

use crate::app::identity::load_or_create_identity;
use crate::app::sink::EventSink;
use crate::app::trust::remember_peer;
use crate::cli_error::CliError;
use crate::config::SessionConfig;
use crate::local_state::local_state_file_path;
use crate::runtime::handshake::exchange_hello;
use crate::runtime::machine::run_machine_join_session;
use crate::runtime::pipe::run_pipe_join_session;

pub(crate) async fn run_connected_session(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: &IrohTransport,
    connection: IrohConnection,
    is_host: bool,
) -> Result<(), CliError> {
    transport
        .enforce_connection_policy(&connection)
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let session_id = config.room_id.clone();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::PeerConnected {
                session_id: session_id.clone(),
            })
            .context("failed to record peer connection")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    let (mut send, mut recv): (SendStream, RecvStream) = if is_host {
        connection
            .accept_control_stream()
            .await
            .map_err(|error| CliError::transport(error.to_string()))?
    } else {
        connection
            .open_control_stream()
            .await
            .map_err(|error| CliError::transport(error.to_string()))?
    };

    let identity = load_or_create_identity(&local_state_file_path())
        .map_err(|error| CliError::local_io(error.to_string()))?;
    let local_fingerprint = Some(identity.fingerprint.clone());
    let local_ticket = transport
        .local_ticket()
        .map_err(|error| CliError::transport(error.to_string()))?;
    let peer = exchange_hello(
        config,
        &session_id,
        &mut send,
        &mut recv,
        local_fingerprint.as_deref(),
        Some(&local_ticket.encoded),
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;
    let peer_trust = remember_peer(&peer).map_err(|error| CliError::local_io(error.to_string()))?;

    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Negotiated {
                session_id: session_id.clone(),
            })
            .context("failed to record negotiated state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    sink.emit_runtime_event(RuntimeEvent::HandshakeCompleted { peer: peer.clone() });
    if let Some(trust) = peer_trust.as_ref() {
        sink.emit_runtime_event(RuntimeEvent::PeerTrust {
            status: trust.status.to_string(),
            peer_name: trust.peer_name.clone(),
            peer_fingerprint: trust.peer_fingerprint.clone(),
            previous_name: trust.previous_name.clone(),
        });
    }
    let connection_status = transport.connection_status(&connection).await;
    sink.emit_runtime_event(RuntimeEvent::ConnectionStatus {
        mode: connection_status.mode.to_string(),
        remote_addr: connection_status.remote_addr.clone(),
    });
    if config.is_pipe_mode() {
        run_pipe_join_session(
            config,
            sink,
            transport,
            &connection,
            &mut send,
            &mut recv,
            &peer,
            Some(identity.fingerprint.clone()),
            local_ticket.encoded,
            peer.peer_ticket.clone(),
        )
        .await?;
    } else if !config.machine {
        return Err(CliError::runtime(
            "interactive sessions should use the room TUI adapter",
        ));
    } else {
        debug_assert!(!is_host, "machine host path should use run_machine_host");
        run_machine_join_session(
            config,
            sink,
            transport,
            &connection,
            &mut send,
            &mut recv,
            &peer,
            Some(identity.fingerprint.clone()),
            local_ticket.encoded,
            peer.peer_ticket.clone(),
        )
        .await?;
    }

    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::CloseRequested)
            .context("failed to enter closing state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Closed)
            .context("failed to enter closed state")
            .map_err(|error| CliError::runtime(error.to_string()))?,
    ));

    Ok(())
}
