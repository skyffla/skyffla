use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use iroh::endpoint::{RecvStream, SendStream};
use serde_json::json;
use sha2::{Digest as ShaDigestTrait, Sha256};
use skyffla_protocol::{
    Accept, Cancel, Capabilities, ChatMessage, Complete, ControlMessage, DataStreamHeader,
    Envelope, Hello, HelloAck, Offer, Reject, TransferKind, TransportCapability, PROTOCOL_VERSION,
};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{IrohConnection, IrohTransport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::local_state::update_local_state;
use crate::transfers::{
    accept_pending_offer, describe_offer_size, finalize_sha256_digest, format_digest_suffix,
    send_clipboard, send_path, spawn_outgoing_transfer_task, transfer_kind_label,
    PendingOutgoingTransfer, TransferTaskEvent,
};
use crate::ui::{
    help_lines, parse_user_input, resolve_cancel_target, TerminalUiGuard, TransferStateUi,
    TransferUi, UiState, UserInput,
};
use crate::{
    digest_json, next_message_id, read_data_header, read_envelope, remember_peer,
    send_transfer_error, short_fingerprint, write_data_header, write_envelope, EventSink,
    SessionConfig,
};

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

    let (mut send, mut recv) = if is_host {
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

async fn run_interactive_chat_loop(
    config: &SessionConfig,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    ui: &mut UiState,
) -> Result<()> {
    let _terminal = TerminalUiGuard::activate()?;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let (task_tx, mut task_rx) = mpsc::unbounded_channel();
    let mut pending_outgoing = HashMap::<String, PendingOutgoingTransfer>::new();
    let mut running_transfers = HashMap::<String, Arc<AtomicBool>>::new();
    thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if input_tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });
    let mut send_open = true;
    ui.system("interactive session ready; use /help for commands".to_string());
    ui.system(format!(
        "auto-accept is {} for files and clipboard",
        if ui.auto_accept_enabled { "on" } else { "off" }
    ));
    ui.render();
    let shutdown_signal = wait_for_shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            maybe_key = input_rx.recv() => {
                let Some(key) = maybe_key else {
                    if send_open {
                        shutdown_interactive_session(
                            session_id,
                            send,
                            ui,
                            "input closed",
                            &mut pending_outgoing,
                            &mut running_transfers,
                        ).await?;
                    }
                    break;
                };
                if !send_open {
                    continue;
                }

                if let Some(line) = ui.handle_key_event(key) {
                    match parse_user_input(&line) {
                        UserInput::Quit => {
                            shutdown_interactive_session(
                                session_id,
                                send,
                                ui,
                                "user requested shutdown",
                                &mut pending_outgoing,
                                &mut running_transfers,
                            )
                            .await?;
                            send_open = false;
                        }
                        UserInput::Chat(text) => {
                            if !text.is_empty() {
                                send_chat_message(session_id, send, &text, Some(ui), None).await?;
                            }
                        }
                        UserInput::SendFile(path) => match send_path(session_id, send, &path, ui).await {
                            Ok(plan) => {
                                pending_outgoing.insert(plan.offer.transfer_id.clone(), plan);
                            }
                            Err(error) => ui.system(format!("send failed: {error:#}")),
                        },
                        UserInput::SendClipboard => match send_clipboard(session_id, send, ui).await {
                            Ok(plan) => {
                                pending_outgoing.insert(plan.offer.transfer_id.clone(), plan);
                            }
                            Err(error) => ui.system(format!("clipboard send failed: {error:#}")),
                        },
                        UserInput::Accept => {
                            if let Some(offer) = ui.pending_offer.clone() {
                                ui.pending_offer = None;
                                match accept_pending_offer(
                                    session_id,
                                    connection,
                                    send,
                                    &config.download_dir,
                                    offer.clone(),
                                    ui,
                                    task_tx.clone(),
                                )
                                .await
                                {
                                    Ok(cancel) => {
                                        running_transfers.insert(offer.transfer_id.clone(), cancel);
                                    }
                                    Err(error) => {
                                        let _ = send_transfer_error(
                                            session_id,
                                            send,
                                            "transfer_failed",
                                            &error.to_string(),
                                            Some(offer.transfer_id.as_str()),
                                        )
                                        .await;
                                        ui.mark_transfer_cancelled(&offer.transfer_id);
                                        ui.system(format!("receive failed for {}: {error:#}", offer.name));
                                    }
                                }
                            } else {
                                ui.system("no pending offer to accept".to_string());
                            }
                        }
                        UserInput::Reject => {
                            if let Some(offer) = ui.pending_offer.take() {
                                write_envelope(
                                    send,
                                    &Envelope::new(
                                        session_id,
                                        next_message_id(),
                                        ControlMessage::Reject(Reject {
                                            transfer_id: offer.transfer_id.clone(),
                                            reason: Some("user rejected".to_string()),
                                        }),
                                    ),
                                )
                                .await?;
                                ui.mark_transfer_rejected(&offer.transfer_id);
                                ui.system(format!("rejected file {}", offer.name));
                            } else {
                                ui.system("no pending offer to reject".to_string());
                            }
                        }
                        UserInput::Cancel(requested) => match resolve_cancel_target(ui, requested.as_deref()) {
                            Ok(Some(transfer_id)) => {
                                cancel_transfer(
                                    session_id,
                                    send,
                                    ui,
                                    &transfer_id,
                                    "user cancelled",
                                )
                                .await?;
                                if let Some(cancel) = running_transfers.remove(&transfer_id) {
                                    cancel.store(true, Ordering::SeqCst);
                                }
                                pending_outgoing.remove(&transfer_id);
                            }
                            Ok(None) => {
                                ui.system("no active transfer to cancel".to_string());
                            }
                            Err(message) => ui.system(message),
                        },
                        UserInput::AutoAccept(enabled) => match enabled {
                            Some(enabled) => {
                                ui.auto_accept_enabled = enabled;
                                update_local_state(&ui.state_path, |state| {
                                    state.auto_accept_enabled = enabled;
                                });
                                ui.system(format!(
                                    "auto-accept {} for files and clipboard",
                                    if enabled { "enabled" } else { "disabled" }
                                ));
                            }
                            None => {
                                ui.system(format!(
                                    "auto-accept is {} for files and clipboard",
                                    if ui.auto_accept_enabled { "on" } else { "off" }
                                ));
                            }
                        },
                        UserInput::Help => {
                            for line in help_lines() {
                                ui.system(line.to_string());
                            }
                        }
                    }
                }
                ui.render();
            }
            envelope = read_envelope(recv) => {
                match envelope? {
                    Some(envelope) => {
                        handle_interactive_envelope(
                            session_id,
                            connection,
                            send,
                            &config.download_dir,
                            ui,
                            task_tx.clone(),
                            &mut pending_outgoing,
                            &mut running_transfers,
                            envelope,
                        ).await?;
                    }
                    None => break,
                }
                ui.render();
            }
            maybe_event = task_rx.recv() => {
                let Some(event) = maybe_event else { break; };
                match event {
                    TransferTaskEvent::Progress { transfer_id, bytes_done, bytes_total } => {
                        ui.update_transfer_progress(&transfer_id, bytes_done, bytes_total);
                    }
                    TransferTaskEvent::LocalComplete { transfer_id, message } => {
                        running_transfers.remove(&transfer_id);
                        ui.mark_transfer_completed(&transfer_id);
                        ui.system(message);
                    }
                    TransferTaskEvent::ReceiveComplete { transfer_id, message, digest } => {
                        running_transfers.remove(&transfer_id);
                        write_envelope(
                            send,
                            &Envelope::new(
                                session_id,
                                next_message_id(),
                                ControlMessage::Complete(Complete {
                                    transfer_id: transfer_id.clone(),
                                    digest: Some(digest),
                                }),
                            ),
                        ).await?;
                        ui.mark_transfer_completed(&transfer_id);
                        ui.system(message);
                    }
                    TransferTaskEvent::Failed { transfer_id, message, notify_peer } => {
                        running_transfers.remove(&transfer_id);
                        ui.mark_transfer_cancelled(&transfer_id);
                        ui.system(message.clone());
                        if notify_peer {
                            let _ = send_transfer_error(
                                session_id,
                                send,
                                "transfer_failed",
                                &message,
                                Some(transfer_id.as_str()),
                            ).await;
                        }
                    }
                }
                ui.render();
            }
            _ = &mut shutdown_signal => {
                if send_open {
                    shutdown_interactive_session(
                        session_id,
                        send,
                        ui,
                        "terminal disconnected",
                        &mut pending_outgoing,
                        &mut running_transfers,
                    ).await?;
                    ui.render();
                }
                break;
            }
        }
    }

    Ok(())
}

async fn run_stdio_session(
    config: &SessionConfig,
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<()> {
    match config.role {
        crate::Role::Host => run_stdio_sender(sink, session_id, connection, send, recv).await,
        crate::Role::Join => run_stdio_receiver(sink, session_id, connection, send, recv).await,
    }
}

async fn run_stdio_sender(
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<()> {
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
    .await?;
    sink.emit_json_event(json!({
        "event": "offer",
        "transfer_id": transfer_id,
        "kind": "stdio",
        "name": "stdio",
    }));

    loop {
        let envelope = read_envelope(recv)
            .await?
            .context("peer closed control stream while stdio offer was pending")?;
        match envelope.payload {
            ControlMessage::Accept(Accept {
                transfer_id: accepted,
            }) if accepted == transfer_id => {
                break;
            }
            ControlMessage::Reject(reject) if reject.transfer_id == transfer_id => {
                bail!(
                    "stdio transfer rejected{}",
                    reject
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
            ControlMessage::Cancel(cancel) if cancel.transfer_id == transfer_id => {
                bail!(
                    "stdio transfer cancelled{}",
                    cancel
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
            ControlMessage::Error(error) if error.transfer_id.as_deref() == Some(&transfer_id) => {
                bail!("stdio transfer error {}: {}", error.code, error.message);
            }
            other => bail!(
                "unexpected control message while waiting for stdio accept: {:?}",
                other
            ),
        }
    }

    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.clone(),
            kind: TransferKind::Stdio,
        },
    )
    .await?;

    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    loop {
        let bytes_read = stdin
            .read(&mut buffer)
            .await
            .context("failed to read stdin for stdio transfer")?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to write stdio payload bytes")?;
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
        .context("failed to finish stdio data stream")?;

    loop {
        let envelope = read_envelope(recv)
            .await?
            .context("peer closed control stream before stdio completion")?;
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
                bail!(
                    "stdio transfer cancelled{}",
                    cancel
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
            ControlMessage::Error(error) if error.transfer_id.as_deref() == Some(&transfer_id) => {
                bail!("stdio transfer error {}: {}", error.code, error.message);
            }
            other => bail!(
                "unexpected control message while waiting for stdio complete: {:?}",
                other
            ),
        }
    }
}

async fn run_stdio_receiver(
    sink: &EventSink,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<()> {
    let offer = loop {
        let envelope = read_envelope(recv)
            .await?
            .context("peer closed control stream before stdio offer")?;
        match envelope.payload {
            ControlMessage::Offer(offer) if offer.kind == TransferKind::Stdio => break offer,
            ControlMessage::Error(error) => bail!("peer error {}: {}", error.code, error.message),
            other => bail!("expected stdio offer, got {:?}", other),
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
    .await?;

    let (_, mut data_recv) = connection.accept_data_stream().await?;
    let header = read_data_header(&mut data_recv)
        .await?
        .context("peer closed data stream before sending a stdio header")?;
    if header.transfer_id != offer.transfer_id || header.kind != TransferKind::Stdio {
        bail!("received unexpected stdio data stream header");
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
                    .context("failed to write stdio payload to stdout")?;
                stdout.flush().await.context("failed to flush stdout")?;
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
            Err(error) => return Err(error).context("failed reading stdio payload stream"),
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
    .await?;
    sink.emit_json_event(json!({
        "event": "complete",
        "transfer_id": offer.transfer_id,
        "digest": digest_json(Some(&digest)),
    }));
    Ok(())
}

async fn exchange_hello(
    config: &SessionConfig,
    session_id: &str,
    send: &mut SendStream,
    recv: &mut RecvStream,
    local_fingerprint: Option<&str>,
) -> Result<SessionPeer> {
    let hello = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            session_id: session_id.to_string(),
            peer_name: config.peer_name.clone(),
            peer_fingerprint: local_fingerprint.map(ToOwned::to_owned),
            capabilities: Capabilities::default(),
            transport_capabilities: vec![TransportCapability::NativeDirect],
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
            SessionPeer {
                session_id: hello.session_id,
                peer_name: hello.peer_name,
                peer_fingerprint: hello.peer_fingerprint,
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

async fn send_chat_message(
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

async fn handle_interactive_envelope(
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    download_dir: &Path,
    ui: &mut UiState,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
    pending_outgoing: &mut HashMap<String, PendingOutgoingTransfer>,
    running_transfers: &mut HashMap<String, Arc<AtomicBool>>,
    envelope: Envelope,
) -> Result<()> {
    match envelope.payload {
        ControlMessage::ChatMessage(message) => {
            let text = message.text;
            let speaker = ui.peer_name.clone();
            ui.chat(&speaker, &text);
            Ok(())
        }
        ControlMessage::Accept(Accept { transfer_id }) => {
            if let Some(plan) = pending_outgoing.remove(&transfer_id) {
                ui.mark_transfer_streaming(&transfer_id);
                running_transfers.insert(transfer_id.clone(), plan.cancel.clone());
                spawn_outgoing_transfer_task(connection.clone(), plan, task_tx);
            } else {
                ui.system(format!("transfer {} accepted", transfer_id));
            }
            Ok(())
        }
        ControlMessage::Offer(offer)
            if matches!(
                offer.kind,
                TransferKind::File | TransferKind::FolderArchive | TransferKind::Clipboard
            ) =>
        {
            ui.upsert_transfer(TransferUi {
                id: offer.transfer_id.clone(),
                state: TransferStateUi::AwaitingDecision,
                bytes_done: 0,
                bytes_total: offer.size,
            });
            let should_auto_accept = ui.auto_accept_enabled
                && matches!(offer.kind, TransferKind::File | TransferKind::Clipboard);
            if should_auto_accept {
                ui.system(format!(
                    "auto-accepting {} {}{}",
                    transfer_kind_label(&offer.kind),
                    offer.name,
                    describe_offer_size(&offer)
                ));
                match accept_pending_offer(
                    session_id,
                    connection,
                    send,
                    download_dir,
                    offer.clone(),
                    ui,
                    task_tx.clone(),
                )
                .await
                {
                    Ok(cancel) => {
                        running_transfers.insert(offer.transfer_id.clone(), cancel);
                    }
                    Err(error) => {
                        let _ = send_transfer_error(
                            session_id,
                            send,
                            "transfer_failed",
                            &error.to_string(),
                            Some(offer.transfer_id.as_str()),
                        )
                        .await;
                        ui.mark_transfer_cancelled(&offer.transfer_id);
                        ui.system(format!("auto-accept failed for {}: {error:#}", offer.name));
                    }
                }
            } else {
                ui.pending_offer = Some(offer.clone());
                ui.system(format!(
                    "incoming {} offer: {}{}. /accept or /reject",
                    transfer_kind_label(&offer.kind),
                    offer.name,
                    describe_offer_size(&offer)
                ));
            }
            Ok(())
        }
        ControlMessage::Complete(complete) => {
            pending_outgoing.remove(&complete.transfer_id);
            running_transfers.remove(&complete.transfer_id);
            ui.mark_transfer_completed(&complete.transfer_id);
            ui.system(format!(
                "transfer {} complete{}",
                complete.transfer_id,
                format_digest_suffix(complete.digest.as_ref())
            ));
            Ok(())
        }
        ControlMessage::Reject(reject) => {
            pending_outgoing.remove(&reject.transfer_id);
            if let Some(cancel) = running_transfers.remove(&reject.transfer_id) {
                cancel.store(true, Ordering::SeqCst);
            }
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
            pending_outgoing.remove(&cancel.transfer_id);
            if let Some(flag) = running_transfers.remove(&cancel.transfer_id) {
                flag.store(true, Ordering::SeqCst);
            }
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
                pending_outgoing.remove(transfer_id);
                if let Some(flag) = running_transfers.remove(transfer_id) {
                    flag.store(true, Ordering::SeqCst);
                }
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

async fn handle_post_handshake_message(
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

async fn shutdown_interactive_session(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    reason: &str,
    pending_outgoing: &mut HashMap<String, PendingOutgoingTransfer>,
    running_transfers: &mut HashMap<String, Arc<AtomicBool>>,
) -> Result<()> {
    let cancelled = cancel_active_transfers(
        session_id,
        send,
        ui,
        reason,
        pending_outgoing,
        running_transfers,
    )
    .await?;
    if cancelled > 0 {
        ui.system(format!(
            "cancelled {} transfer(s) before closing",
            cancelled
        ));
    }
    send.finish()
        .context("failed to finish control stream send side")?;
    ui.system("closing session".to_string());
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("failed to register SIGHUP handler");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sighup.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    std::future::pending::<()>().await;
}

async fn cancel_active_transfers(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    reason: &str,
    pending_outgoing: &mut HashMap<String, PendingOutgoingTransfer>,
    running_transfers: &mut HashMap<String, Arc<AtomicBool>>,
) -> Result<usize> {
    let transfer_ids = ui.cancellable_transfer_ids();
    for transfer_id in &transfer_ids {
        cancel_transfer(session_id, send, ui, transfer_id, reason).await?;
        pending_outgoing.remove(transfer_id);
        if let Some(cancel) = running_transfers.remove(transfer_id) {
            cancel.store(true, Ordering::SeqCst);
        }
    }
    Ok(transfer_ids.len())
}

async fn cancel_transfer(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    transfer_id: &str,
    reason: &str,
) -> Result<()> {
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Cancel(Cancel {
                transfer_id: transfer_id.to_string(),
                reason: Some(reason.to_string()),
            }),
        ),
    )
    .await?;
    ui.mark_transfer_cancelled(transfer_id);
    if ui
        .pending_offer
        .as_ref()
        .is_some_and(|offer| offer.transfer_id == transfer_id)
    {
        ui.pending_offer = None;
    }
    ui.system(format!("transfer {} cancelled ({})", transfer_id, reason));
    Ok(())
}
