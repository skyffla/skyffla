use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use serde_json::json;
use sha2::{Digest as ShaDigestTrait, Sha256};
use skyffla_protocol::{
    Accept, Complete, ControlMessage, DataStreamHeader, Envelope, Offer, TransferKind,
};
use skyffla_transport::IrohConnection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::app::sink::{digest_json, EventSink};
use crate::cli_error::CliError;
use crate::config::{Role, SessionConfig};
use crate::net::framing::{
    next_message_id, read_data_header, read_envelope, write_data_header, write_envelope,
};
use crate::transfers::finalize_sha256_digest;

pub(crate) async fn run_stdio_session(
    config: &SessionConfig,
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CliError> {
    match config.role {
        Role::Host => run_stdio_sender(sink, session_id, connection, send, recv).await,
        Role::Join => run_stdio_receiver(sink, session_id, connection, send, recv).await,
    }
}

async fn run_stdio_sender(
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CliError> {
    let transfer_id = format!("t{}", next_message_id());
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::Stdio,
        name: "stdio".to_string(),
        size: None,
        mime: Some("application/octet-stream".to_string()),
        item_count: None,
        compression: None,
        path_hint: None,
    };
    write_envelope(
        send,
        &Envelope::new(session_id, next_message_id(), ControlMessage::Offer(offer)),
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;
    sink.emit_json_event(json!({
        "event": "offer",
        "transfer_id": transfer_id,
        "kind": "stdio",
        "name": "stdio",
    }));

    loop {
        let envelope = read_envelope(recv)
            .await
            .map_err(|error| CliError::protocol(error.to_string()))?
            .ok_or_else(|| {
                CliError::protocol("peer closed control stream while stdio offer was pending")
            })?;
        match envelope.payload {
            ControlMessage::Accept(Accept {
                transfer_id: accepted,
            }) if accepted == transfer_id => {
                break;
            }
            ControlMessage::Reject(reject) if reject.transfer_id == transfer_id => {
                return Err(CliError::rejected(format!(
                    "stdio transfer rejected{}",
                    reject
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                )));
            }
            ControlMessage::Cancel(cancel) if cancel.transfer_id == transfer_id => {
                return Err(CliError::cancelled(format!(
                    "stdio transfer cancelled{}",
                    cancel
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                )));
            }
            ControlMessage::Error(error) if error.transfer_id.as_deref() == Some(&transfer_id) => {
                return Err(CliError::peer(format!(
                    "stdio transfer error {}: {}",
                    error.code, error.message
                )));
            }
            other => {
                return Err(CliError::protocol(format!(
                    "unexpected control message while waiting for stdio accept: {:?}",
                    other
                )))
            }
        }
    }

    let (mut data_send, _) = connection
        .open_data_stream()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.clone(),
            kind: TransferKind::Stdio,
        },
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;

    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    loop {
        let bytes_read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin for stdio transfer")
            .map_err(|error| CliError::local_io(error.to_string()))?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to write stdio payload bytes")
            .map_err(|error| CliError::transport(error.to_string()))?;
        bytes_done += bytes_read as u64;
        sink.emit_json_event(json!({
            "event": "progress",
            "transfer_id": transfer_id,
            "bytes_done": bytes_done,
            "bytes_total": serde_json::Value::Null,
        }));
    }
    data_send
        .finish()
        .context("failed to finish stdio data stream")
        .map_err(|error| CliError::transport(error.to_string()))?;

    loop {
        let envelope = read_envelope(recv)
            .await
            .map_err(|error| CliError::protocol(error.to_string()))?
            .ok_or_else(|| {
                CliError::protocol("peer closed control stream before stdio completion")
            })?;
        match envelope.payload {
            ControlMessage::Complete(complete) if complete.transfer_id == transfer_id => {
                sink.emit_json_event(json!({
                    "event": "complete",
                    "transfer_id": transfer_id,
                    "digest": digest_json(complete.digest.as_ref()),
                }));
                return Ok(());
            }
            ControlMessage::Cancel(cancel) if cancel.transfer_id == transfer_id => {
                return Err(CliError::cancelled(format!(
                    "stdio transfer cancelled{}",
                    cancel
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                )));
            }
            ControlMessage::Error(error) if error.transfer_id.as_deref() == Some(&transfer_id) => {
                return Err(CliError::peer(format!(
                    "stdio transfer error {}: {}",
                    error.code, error.message
                )));
            }
            other => {
                return Err(CliError::protocol(format!(
                    "unexpected control message while waiting for stdio complete: {:?}",
                    other
                )))
            }
        }
    }
}

async fn run_stdio_receiver(
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CliError> {
    let offer = loop {
        let envelope = read_envelope(recv)
            .await
            .map_err(|error| CliError::protocol(error.to_string()))?
            .ok_or_else(|| CliError::protocol("peer closed control stream before stdio offer"))?;
        match envelope.payload {
            ControlMessage::Offer(offer) if offer.kind == TransferKind::Stdio => break offer,
            ControlMessage::Error(error) => {
                return Err(CliError::peer(format!(
                    "peer error {}: {}",
                    error.code, error.message
                )))
            }
            other => {
                return Err(CliError::protocol(format!(
                    "expected stdio offer, got {:?}",
                    other
                )))
            }
        }
    };

    sink.emit_json_event(json!({
        "event": "offer",
        "transfer_id": offer.transfer_id,
        "kind": "stdio",
        "name": offer.name,
    }));
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Accept(Accept {
                transfer_id: offer.transfer_id.clone(),
            }),
        ),
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;

    let (_, mut data_recv) = connection
        .accept_data_stream()
        .await
        .map_err(|error| CliError::transport(error.to_string()))?;
    let header = read_data_header(&mut data_recv)
        .await
        .map_err(|error| CliError::protocol(error.to_string()))?
        .ok_or_else(|| {
            CliError::protocol("peer closed data stream before sending a stdio header")
        })?;
    if header.transfer_id != offer.transfer_id || header.kind != TransferKind::Stdio {
        return Err(CliError::protocol(
            "received unexpected stdio data stream header",
        ));
    }

    let mut stdout = tokio::io::stdout();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        match data_recv.read(&mut buffer).await {
            Ok(Some(bytes_read)) => {
                stdout
                    .write_all(&buffer[..bytes_read])
                    .await
                    .context("failed to write stdio payload to stdout")
                    .map_err(|error| CliError::local_io(error.to_string()))?;
                stdout
                    .flush()
                    .await
                    .context("failed to flush stdout")
                    .map_err(|error| CliError::local_io(error.to_string()))?;
                hasher.update(&buffer[..bytes_read]);
                bytes_done += bytes_read as u64;
                sink.emit_json_event(json!({
                    "event": "progress",
                    "transfer_id": offer.transfer_id,
                    "bytes_done": bytes_done,
                    "bytes_total": serde_json::Value::Null,
                }));
            }
            Ok(None) => break,
            Err(error) => {
                return Err(CliError::transport(format!(
                    "failed reading stdio payload stream: {error}"
                )))
            }
        }
    }

    let digest = finalize_sha256_digest(hasher);
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Complete(Complete {
                transfer_id: offer.transfer_id.clone(),
                digest: Some(digest.clone()),
            }),
        ),
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))?;
    sink.emit_json_event(json!({
        "event": "complete",
        "transfer_id": offer.transfer_id,
        "digest": digest_json(Some(&digest)),
    }));
    Ok(())
}
