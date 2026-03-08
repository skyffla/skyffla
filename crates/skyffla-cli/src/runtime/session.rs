use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_session::{state_changed_event, RuntimeEvent, SessionEvent, SessionMachine};
use skyffla_transport::{IrohConnection, IrohTransport};

use crate::app::sink::EventSink;
use crate::app::trust::{remember_peer, short_fingerprint};
use crate::config::SessionConfig;
use crate::net::framing::read_envelope;
use crate::runtime::handshake::{exchange_hello, handle_post_handshake_message, send_chat_message};
use crate::runtime::interactive::run_interactive_chat_loop;
use crate::runtime::stdio::run_stdio_session;
use crate::ui::UiState;

pub(crate) async fn run_connected_session(
    config: &SessionConfig,
    sink: &EventSink,
    session: &mut SessionMachine,
    transport: &IrohTransport,
    connection: IrohConnection,
    is_host: bool,
) -> Result<()> {
    let session_id = config.stream_id.clone();
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::PeerConnected {
                session_id: session_id.clone(),
            })
            .context("failed to record peer connection")?,
    ));

    let (mut send, mut recv): (SendStream, RecvStream) = if is_host {
        connection.accept_control_stream().await?
    } else {
        connection.open_control_stream().await?
    };

    let local_fingerprint = short_fingerprint(&transport.endpoint().id().to_string());
    let peer = exchange_hello(
        config,
        &session_id,
        &mut send,
        &mut recv,
        local_fingerprint.as_deref(),
    )
    .await?;
    let peer_trust = remember_peer(&peer);

    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Negotiated {
                session_id: session_id.clone(),
                stdio: config.stdio,
            })
            .context("failed to record negotiated state")?,
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
    let mut ui = UiState::new(
        &session_id,
        &config.peer_name,
        &transport.endpoint().id().to_string(),
        config.auto_accept_policy.clone(),
        config.auto_accept_source,
    );
    ui.peer_name = peer.peer_name.clone();
    ui.system(format!(
        "session stream={} you={} peer={}",
        ui.stream_id, ui.local_name, ui.peer_name
    ));
    ui.system(format!("connected to {}", ui.peer_name));
    ui.system(format!(
        "connection {} remote={}",
        connection_status.mode,
        connection_status
            .remote_addr
            .as_deref()
            .unwrap_or("unknown")
    ));
    if let Some(trust) = peer_trust {
        ui.system(trust.message);
    }

    if let Some(message) = &config.outgoing_message {
        send_chat_message(&session_id, &mut send, message, None, Some(sink)).await?;
        ui.chat("you", message);
        send.finish()
            .context("failed to finish control stream send side")?;

        while let Some(envelope) = read_envelope(&mut recv).await? {
            handle_post_handshake_message(&mut ui, envelope, Some(sink)).await?;
        }
    } else if config.stdio {
        run_stdio_session(config, sink, &session_id, &connection, &mut send, &mut recv).await?;
    } else {
        run_interactive_chat_loop(
            config,
            &session_id,
            &connection,
            &mut send,
            &mut recv,
            &mut ui,
        )
        .await?;
    }

    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::CloseRequested)
            .context("failed to enter closing state")?,
    ));
    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Closed)
            .context("failed to enter closed state")?,
    ));

    Ok(())
}
