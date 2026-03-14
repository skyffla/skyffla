use anyhow::{bail, Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::{
    Capabilities, ChatMessage, ControlMessage, Envelope, Hello, HelloAck, TransferKind,
    TransportCapability, PROTOCOL_VERSION,
};
use skyffla_session::{RuntimeEvent, SessionPeer};

use crate::app::sink::EventSink;
use crate::config::SessionConfig;
use crate::net::framing::{next_message_id, read_envelope, write_envelope};
use crate::transfers::{describe_offer_size, format_digest_suffix, transfer_kind_label};
use crate::ui::{TransferStateUi, TransferUi, UiState};

pub(crate) async fn exchange_hello(
    config: &SessionConfig,
    session_id: &str,
    send: &mut SendStream,
    recv: &mut RecvStream,
    local_fingerprint: Option<&str>,
    local_ticket: Option<&str>,
) -> Result<SessionPeer> {
    let hello = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            session_id: session_id.to_string(),
            peer_name: config.peer_name.clone(),
            peer_fingerprint: local_fingerprint.map(ToOwned::to_owned),
            peer_ticket: local_ticket.map(ToOwned::to_owned),
            capabilities: Capabilities::default(),
            transport_capabilities: vec![TransportCapability::NativeDirect],
            session_mode: config.session_mode(),
        }),
    );
    write_envelope(send, &hello).await?;

    let peer_hello = read_envelope(recv)
        .await?
        .context("peer closed control stream before sending hello")?;
    let peer = match peer_hello.payload {
        ControlMessage::Hello(hello) => {
            if hello.protocol_version != PROTOCOL_VERSION {
                bail!(
                    "protocol version mismatch: local {}, peer {}",
                    PROTOCOL_VERSION,
                    hello.protocol_version
                );
            }
            if hello.session_mode != config.session_mode() {
                bail!(
                    "session mode mismatch: local {:?}, peer {:?}",
                    config.session_mode(),
                    hello.session_mode
                );
            }
            SessionPeer {
                session_id: hello.session_id,
                peer_name: hello.peer_name,
                peer_fingerprint: hello.peer_fingerprint,
                peer_ticket: hello.peer_ticket,
            }
        }
        other => bail!("expected hello from peer, got {:?}", other),
    };

    let ack = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::HelloAck(HelloAck {
            protocol_version: PROTOCOL_VERSION,
            session_id: session_id.to_string(),
        }),
    );
    write_envelope(send, &ack).await?;

    let peer_ack = read_envelope(recv)
        .await?
        .context("peer closed control stream before sending hello ack")?;
    match peer_ack.payload {
        ControlMessage::HelloAck(ack) if ack.protocol_version == PROTOCOL_VERSION => Ok(peer),
        ControlMessage::HelloAck(ack) => bail!(
            "protocol version mismatch in hello ack: local {}, peer {}",
            PROTOCOL_VERSION,
            ack.protocol_version
        ),
        other => bail!("expected hello ack from peer, got {:?}", other),
    }
}

pub(crate) async fn send_chat_message(
    session_id: &str,
    send: &mut SendStream,
    text: &str,
    ui: Option<&mut UiState>,
    sink: Option<&EventSink>,
) -> Result<()> {
    let envelope = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::ChatMessage(ChatMessage {
            text: text.to_string(),
        }),
    );
    write_envelope(send, &envelope).await?;
    if let Some(ui) = ui {
        ui.chat("you", text);
    }
    if let Some(sink) = sink {
        sink.emit_runtime_event(RuntimeEvent::ChatSent {
            text: text.to_string(),
        });
    }
    Ok(())
}

pub(crate) async fn handle_post_handshake_message(
    ui: &mut UiState,
    envelope: Envelope,
    sink: Option<&EventSink>,
) -> Result<()> {
    match envelope.payload {
        ControlMessage::ChatMessage(message) => {
            let text = message.text;
            if let Some(sink) = sink {
                sink.emit_runtime_event(RuntimeEvent::ChatReceived { text: text.clone() });
            }
            let speaker = ui.peer_name.clone();
            ui.chat(&speaker, &text);
            Ok(())
        }
        ControlMessage::Offer(offer)
            if matches!(
                offer.kind,
                TransferKind::File | TransferKind::FolderArchive | TransferKind::Clipboard
            ) =>
        {
            ui.pending_offer = Some(offer.clone());
            ui.upsert_transfer(TransferUi {
                id: offer.transfer_id.clone(),
                state: TransferStateUi::AwaitingDecision,
                bytes_done: 0,
                bytes_total: offer.size,
            });
            ui.system(format!(
                "incoming {} offer: {}{}. /accept or /reject",
                transfer_kind_label(&offer.kind),
                offer.name,
                describe_offer_size(&offer)
            ));
            Ok(())
        }
        ControlMessage::Complete(complete) => {
            ui.mark_transfer_completed(&complete.transfer_id);
            ui.system(format!(
                "transfer {} complete{}",
                complete.transfer_id,
                format_digest_suffix(complete.digest.as_ref())
            ));
            Ok(())
        }
        ControlMessage::Reject(reject) => {
            ui.mark_transfer_rejected(&reject.transfer_id);
            ui.system(format!(
                "transfer {} rejected{}",
                reject.transfer_id,
                reject
                    .reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        ControlMessage::Cancel(cancel) => {
            ui.mark_transfer_cancelled(&cancel.transfer_id);
            ui.system(format!(
                "transfer {} cancelled{}",
                cancel.transfer_id,
                cancel
                    .reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        ControlMessage::Error(error) => {
            if let Some(transfer_id) = error.transfer_id.as_deref() {
                ui.mark_transfer_cancelled(transfer_id);
                ui.system(format!(
                    "transfer {} error {}: {}",
                    transfer_id, error.code, error.message
                ));
            } else {
                ui.system(format!("peer error {}: {}", error.code, error.message));
            }
            Ok(())
        }
        other => bail!("unexpected control message after handshake: {:?}", other),
    }
}
