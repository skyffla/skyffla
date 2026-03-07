use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use arboard::Clipboard;
use clap::{Args, Parser, Subcommand};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType, ScrollUp,
};
use iroh::endpoint::{ReadError, ReadExactError, RecvStream, SendStream};
use reqwest::Client;
use serde_json::json;
use sha2::{Digest as ShaDigestTrait, Sha256};
use skyffla_protocol::{
    decode_frame, encode_frame, Accept, Cancel, Capabilities, ChatMessage, Complete, Compression,
    ControlMessage, DataStreamHeader, Digest, Envelope, ErrorMessage, Hello, HelloAck, Offer,
    Reject, TransferKind, TransportCapability, PROTOCOL_VERSION,
};
use skyffla_rendezvous::{GetStreamResponse, PutStreamRequest, DEFAULT_TTL_SECONDS};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{IrohConnection, IrohTransport, PeerTicket};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::task;

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Parser)]
#[command(name = "skyffla")]
#[command(about = "Minimal Skyffla peer CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Host(SessionArgs),
    Join(SessionArgs),
}

#[derive(Args, Clone)]
struct SessionArgs {
    stream_id: Option<String>,
    #[arg(
        long,
        env = "SKYFFLA_RENDEZVOUS_URL",
        default_value = "http://127.0.0.1:8080"
    )]
    server: String,
    #[arg(long, default_value = ".")]
    download_dir: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    message: Option<String>,
    #[arg(long)]
    stdio: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy)]
enum Role {
    Host,
    Join,
}

struct SessionConfig {
    role: Role,
    stream_id: String,
    rendezvous_server: String,
    download_dir: PathBuf,
    peer_name: String,
    outgoing_message: Option<String>,
    stdio: bool,
    json_events: bool,
}

#[derive(Clone, Copy)]
struct EventSink {
    stdio: bool,
    json: bool,
}

enum UserInput {
    Chat(String),
    SendFile(PathBuf),
    SendClipboard,
    Accept,
    Reject,
    Cancel(Option<String>),
    AutoAccept(Option<bool>),
    Help,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TransferStateUi {
    Pending,
    AwaitingDecision,
    Streaming,
    Completed,
    Rejected,
    Cancelled,
}

#[derive(Clone, Debug)]
struct TransferUi {
    id: String,
    state: TransferStateUi,
    bytes_done: u64,
    bytes_total: Option<u64>,
}

struct EventLine {
    timestamp: String,
    text: String,
}

#[derive(Clone)]
struct FolderStats {
    item_count: u64,
    total_bytes: u64,
}

#[derive(Clone)]
struct PendingOutgoingTransfer {
    offer: Offer,
    source: OutgoingTransferSource,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
enum OutgoingTransferSource {
    File {
        path: PathBuf,
        file_name: String,
        size: u64,
    },
    Folder {
        path: PathBuf,
        folder_name: String,
        stats: FolderStats,
    },
    Clipboard {
        text: String,
    },
}

enum TransferTaskEvent {
    Progress {
        transfer_id: String,
        bytes_done: u64,
        bytes_total: Option<u64>,
    },
    LocalComplete {
        transfer_id: String,
        message: String,
    },
    ReceiveComplete {
        transfer_id: String,
        message: String,
        digest: Digest,
    },
    Failed {
        transfer_id: String,
        message: String,
        notify_peer: bool,
    },
}

struct UiState {
    stream_id: String,
    peer_name: String,
    local_name: String,
    auto_accept_enabled: bool,
    events: Vec<EventLine>,
    transfers: Vec<TransferUi>,
    pending_offer: Option<Offer>,
    input_buffer: String,
    cursor_index: usize,
    input_history: Vec<String>,
    history_index: Option<usize>,
    draft_buffer: Option<String>,
    history_path: Option<PathBuf>,
    rendered_event_lines: usize,
    rendered_width: Option<usize>,
    next_event_row: u16,
}

struct TerminalUiGuard;

impl TerminalUiGuard {
    fn activate() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = std::io::stdout();
        write!(stdout, "\x1b[?1049h")?;
        stdout.flush().context("failed to initialize terminal UI")?;
        Ok(Self)
    }
}

impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Host(args) => run_host(args).await,
        Command::Join(args) => run_join(args).await,
    }
}

async fn run_host(args: SessionArgs) -> Result<()> {
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

async fn run_join(args: SessionArgs) -> Result<()> {
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

async fn register_stream(
    client: &Client,
    config: &SessionConfig,
    ticket: &PeerTicket,
) -> Result<()> {
    client
        .put(stream_url(config))
        .json(&PutStreamRequest {
            ticket: ticket.encoded.clone(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        })
        .send()
        .await
        .context("failed to register stream with rendezvous server")?
        .error_for_status()
        .context("rendezvous server rejected stream registration")?;
    Ok(())
}

async fn resolve_stream(client: &Client, config: &SessionConfig) -> Result<GetStreamResponse> {
    client
        .get(stream_url(config))
        .send()
        .await
        .context("failed to resolve stream from rendezvous server")?
        .error_for_status()
        .context("rendezvous server rejected stream lookup")?
        .json()
        .await
        .context("failed to decode rendezvous lookup response")
}

async fn delete_stream(client: &Client, config: &SessionConfig) -> Result<()> {
    client
        .delete(stream_url(config))
        .send()
        .await
        .context("failed to delete stream from rendezvous server")?
        .error_for_status()
        .context("rendezvous server rejected stream deletion")?;
    Ok(())
}

async fn run_connected_session(
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

    sink.emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Negotiated {
                session_id: session_id.clone(),
                stdio: config.stdio,
            })
            .context("failed to record negotiated state")?,
    ));
    sink.emit_runtime_event(RuntimeEvent::HandshakeCompleted { peer: peer.clone() });
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
                            UserInput::SendFile(path) => {
                                match send_path(session_id, send, &path, ui).await {
                                    Ok(plan) => {
                                        pending_outgoing.insert(plan.offer.transfer_id.clone(), plan);
                                    }
                                    Err(error) => ui.system(format!("send failed: {error:#}")),
                                }
                            }
                            UserInput::SendClipboard => {
                                match send_clipboard(session_id, send, ui).await {
                                    Ok(plan) => {
                                        pending_outgoing.insert(plan.offer.transfer_id.clone(), plan);
                                    }
                                    Err(error) => ui.system(format!("clipboard send failed: {error:#}")),
                                }
                            }
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
                                            ui.system(format!(
                                                "receive failed for {}: {error:#}",
                                                offer.name
                                            ));
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
                                    ).await?;
                                    ui.mark_transfer_rejected(&offer.transfer_id);
                                    ui.system(format!("rejected file {}", offer.name));
                                } else {
                                    ui.system("no pending offer to reject".to_string());
                                }
                            }
                            UserInput::Cancel(requested) => {
                                match resolve_cancel_target(ui, requested.as_deref()) {
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
                                }
                            }
                            UserInput::AutoAccept(enabled) => {
                                match enabled {
                                    Some(enabled) => {
                                        ui.auto_accept_enabled = enabled;
                                        save_auto_accept(enabled);
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
                                }
                            }
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

async fn send_file(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("failed to derive file name from {}", path.display()))?;
    let transfer_id = format!("t{}", next_message_id());
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::File,
        name: file_name.clone(),
        size: Some(metadata.len()),
        mime: None,
        item_count: None,
        compression: None,
        path_hint: Some(file_name.clone()),
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(metadata.len()),
    });
    ui.system(format!("offered file {}", file_name));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::File {
            path: path.to_path_buf(),
            file_name,
            size: metadata.len(),
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

async fn send_path(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.is_dir() {
        send_folder(session_id, send, path, ui).await
    } else if metadata.is_file() {
        send_file(session_id, send, path, ui).await
    } else {
        bail!(
            "{} is neither a regular file nor a directory",
            path.display()
        )
    }
}

async fn send_folder(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }

    let folder_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("failed to derive folder name from {}", path.display()))?;
    let stats = collect_folder_stats(path)?;
    let transfer_id = format!("t{}", next_message_id());
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::FolderArchive,
        name: folder_name.clone(),
        size: Some(stats.total_bytes),
        mime: None,
        item_count: Some(stats.item_count),
        compression: Some(Compression::Tar),
        path_hint: Some(folder_name.clone()),
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(stats.total_bytes),
    });
    ui.system(format!(
        "offered folder {} ({} items, {} bytes)",
        folder_name, stats.item_count, stats.total_bytes
    ));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::Folder {
            path: path.to_path_buf(),
            folder_name,
            stats,
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

async fn send_clipboard(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let clipboard_text = read_clipboard_text().await?;
    let transfer_id = format!("t{}", next_message_id());
    let bytes = clipboard_text.as_bytes();
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::Clipboard,
        name: "clipboard".to_string(),
        size: Some(bytes.len() as u64),
        mime: Some("text/plain".to_string()),
        item_count: None,
        compression: None,
        path_hint: None,
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(bytes.len() as u64),
    });
    ui.system(format!("offered clipboard text ({} bytes)", bytes.len()));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::Clipboard {
            text: clipboard_text,
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

async fn accept_pending_offer(
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    download_dir: &Path,
    offer: Offer,
    ui: &mut UiState,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<Arc<AtomicBool>> {
    let item_name = offer.name.clone();
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
    ui.mark_transfer_streaming(&offer.transfer_id);
    ui.system(format!(
        "accepting {} {}",
        transfer_kind_label(&offer.kind),
        item_name
    ));
    ui.render();

    fs::create_dir_all(download_dir)
        .await
        .with_context(|| format!("failed to create {}", download_dir.display()))?;
    let cancel = Arc::new(AtomicBool::new(false));
    spawn_incoming_transfer_task(
        connection.clone(),
        download_dir.to_path_buf(),
        offer,
        cancel.clone(),
        task_tx,
    );
    Ok(cancel)
}

fn spawn_outgoing_transfer_task(
    connection: IrohConnection,
    plan: PendingOutgoingTransfer,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) {
    task::spawn(async move {
        let result = match &plan.source {
            OutgoingTransferSource::File {
                path,
                file_name,
                size,
            } => {
                run_outgoing_file_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    path,
                    file_name,
                    *size,
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
            OutgoingTransferSource::Folder {
                path,
                folder_name,
                stats,
            } => {
                run_outgoing_folder_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    path,
                    folder_name,
                    stats.clone(),
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
            OutgoingTransferSource::Clipboard { text } => {
                run_outgoing_clipboard_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    text,
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
        };

        if let Err(error) = result {
            let _ = task_tx.send(TransferTaskEvent::Failed {
                transfer_id: plan.offer.transfer_id.clone(),
                message: format!("transfer {} failed: {error:#}", plan.offer.transfer_id),
                notify_peer: false,
            });
        }
    });
}

fn spawn_incoming_transfer_task(
    connection: IrohConnection,
    download_dir: PathBuf,
    offer: Offer,
    cancel: Arc<AtomicBool>,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) {
    task::spawn(async move {
        if let Err(error) =
            run_incoming_transfer(connection, download_dir, offer.clone(), cancel, &task_tx).await
        {
            let _ = task_tx.send(TransferTaskEvent::Failed {
                transfer_id: offer.transfer_id.clone(),
                message: format!("receive failed for {}: {error:#}", offer.name),
                notify_peer: true,
            });
        }
    });
}

async fn run_outgoing_file_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    path: &Path,
    file_name: &str,
    size: u64,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::File,
        },
    )
    .await?;

    let mut file = File::open(path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes_read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to stream file bytes")?;
        hasher.update(&buffer[..bytes_read]);
        bytes_done += bytes_read as u64;
        let _ = task_tx.send(TransferTaskEvent::Progress {
            transfer_id: transfer_id.to_string(),
            bytes_done,
            bytes_total: Some(size),
        });
    }
    if !cancel.load(Ordering::SeqCst) {
        data_send.finish().context("failed to finish data stream")?;
        let digest = finalize_sha256_digest(hasher);
        let _ = task_tx.send(TransferTaskEvent::LocalComplete {
            transfer_id: transfer_id.to_string(),
            message: format!(
                "sent file {}{}",
                file_name,
                format_digest_suffix(Some(&digest))
            ),
        });
    }
    Ok(())
}

async fn run_outgoing_folder_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    path: &Path,
    folder_name: &str,
    stats: FolderStats,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut child = TokioCommand::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(parent)
        .arg(folder_name)
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to start tar for folder transfer")?;
    let mut tar_stdout = child
        .stdout
        .take()
        .context("failed to capture tar stdout")?;

    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::FolderArchive,
        },
    )
    .await?;

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.start_kill();
            return Ok(());
        }
        let bytes_read = tar_stdout
            .read(&mut buffer)
            .await
            .context("failed to read tar output")?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to stream folder archive bytes")?;
        hasher.update(&buffer[..bytes_read]);
        bytes_done += bytes_read as u64;
        let _ = task_tx.send(TransferTaskEvent::Progress {
            transfer_id: transfer_id.to_string(),
            bytes_done,
            bytes_total: Some(stats.total_bytes),
        });
    }

    let status = child
        .wait()
        .await
        .context("failed waiting for tar sender")?;
    if !status.success() {
        bail!("tar sender exited with status {status}");
    }

    if !cancel.load(Ordering::SeqCst) {
        data_send.finish().context("failed to finish data stream")?;
        let digest = finalize_sha256_digest(hasher);
        let _ = task_tx.send(TransferTaskEvent::LocalComplete {
            transfer_id: transfer_id.to_string(),
            message: format!(
                "sent folder {}{}",
                folder_name,
                format_digest_suffix(Some(&digest))
            ),
        });
    }
    Ok(())
}

async fn run_outgoing_clipboard_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    text: &str,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let bytes = text.as_bytes();
    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::Clipboard,
        },
    )
    .await?;

    if cancel.load(Ordering::SeqCst) {
        return Ok(());
    }
    data_send
        .write_all(bytes)
        .await
        .context("failed to stream clipboard bytes")?;
    data_send.finish().context("failed to finish data stream")?;
    let _ = task_tx.send(TransferTaskEvent::Progress {
        transfer_id: transfer_id.to_string(),
        bytes_done: bytes.len() as u64,
        bytes_total: Some(bytes.len() as u64),
    });
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = finalize_sha256_digest(hasher);
    let _ = task_tx.send(TransferTaskEvent::LocalComplete {
        transfer_id: transfer_id.to_string(),
        message: format!("sent clipboard text{}", format_digest_suffix(Some(&digest))),
    });
    Ok(())
}

async fn run_incoming_transfer(
    connection: IrohConnection,
    download_dir: PathBuf,
    offer: Offer,
    cancel: Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let item_name = offer.name.clone();
    let (_, mut data_recv) = connection.accept_data_stream().await?;
    let header = read_data_header(&mut data_recv)
        .await?
        .context("peer closed data stream before sending a header")?;
    if header.transfer_id != offer.transfer_id || header.kind != offer.kind {
        bail!(
            "received unexpected data stream header for transfer {}",
            offer.transfer_id
        );
    }

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    let completion_label = match offer.kind.clone() {
        TransferKind::File => {
            let target_path = download_dir.join(&item_name);
            let mut output = File::create(&target_path)
                .await
                .with_context(|| format!("failed to create {}", target_path.display()))?;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        output
                            .write_all(&buffer[..bytes_read])
                            .await
                            .with_context(|| {
                                format!("failed to write {}", target_path.display())
                            })?;
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => return Err(error).context("failed to read incoming file stream"),
                }
            }
            target_path.display().to_string()
        }
        TransferKind::FolderArchive => {
            let mut child = TokioCommand::new("tar")
                .arg("-xf")
                .arg("-")
                .arg("-C")
                .arg(&download_dir)
                .stdin(Stdio::piped())
                .spawn()
                .context("failed to start tar for folder extraction")?;
            let mut tar_stdin = child.stdin.take().context("failed to capture tar stdin")?;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.start_kill();
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        tar_stdin
                            .write_all(&buffer[..bytes_read])
                            .await
                            .context("failed to write folder archive bytes to tar")?;
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(error).context("failed to read incoming folder stream");
                    }
                }
            }
            tar_stdin
                .shutdown()
                .await
                .context("failed to finish tar stdin")?;
            drop(tar_stdin);
            let status = child
                .wait()
                .await
                .context("failed waiting for tar extractor")?;
            if !status.success() {
                bail!("tar extractor exited with status {status}");
            }
            download_dir.join(&item_name).display().to_string()
        }
        TransferKind::Clipboard => {
            let mut data = Vec::new();
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        data.extend_from_slice(&buffer[..bytes_read]);
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(error).context("failed to read incoming clipboard stream");
                    }
                }
            }
            let text =
                String::from_utf8(data).context("received clipboard payload was not UTF-8")?;
            write_clipboard_text(text).await?;
            "local clipboard".to_string()
        }
        _ => bail!("unsupported accepted transfer kind: {:?}", offer.kind),
    };

    let digest = finalize_sha256_digest(hasher);
    let _ = task_tx.send(TransferTaskEvent::ReceiveComplete {
        transfer_id: offer.transfer_id.clone(),
        message: format!(
            "received {} {}{}",
            transfer_kind_label(&offer.kind),
            completion_label,
            format_digest_suffix(Some(&digest))
        ),
        digest,
    });
    Ok(())
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

fn resolve_cancel_target(
    ui: &UiState,
    requested: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(transfer_id) => {
            if !ui.has_transfer(transfer_id) {
                return Err(format!("unknown transfer {}", transfer_id));
            }
            Ok(Some(transfer_id.to_string()))
        }
        None => {
            let cancellable = ui.cancellable_transfer_ids();
            match cancellable.len() {
                0 => Ok(None),
                1 => Ok(cancellable.into_iter().next()),
                _ => Err("multiple active transfers; use /cancel <transfer-id>".to_string()),
            }
        }
    }
}

async fn send_transfer_error(
    session_id: &str,
    send: &mut SendStream,
    code: &str,
    message: &str,
    transfer_id: Option<&str>,
) -> Result<()> {
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Error(ErrorMessage {
                code: code.to_string(),
                message: message.to_string(),
                transfer_id: transfer_id.map(ToOwned::to_owned),
            }),
        ),
    )
    .await
}

async fn write_envelope(send: &mut SendStream, envelope: &Envelope) -> Result<()> {
    let bytes = encode_frame(envelope)?;
    send.write_all(&bytes)
        .await
        .context("failed to write envelope bytes")?;
    send.flush()
        .await
        .context("failed to flush envelope bytes")?;
    Ok(())
}

async fn write_data_header(send: &mut SendStream, header: &DataStreamHeader) -> Result<()> {
    let bytes = encode_frame(header)?;
    send.write_all(&bytes)
        .await
        .context("failed to write data header bytes")?;
    send.flush()
        .await
        .context("failed to flush data header bytes")?;
    Ok(())
}

async fn read_envelope(recv: &mut RecvStream) -> Result<Option<Envelope>> {
    read_framed_message(recv, "envelope").await
}

async fn read_data_header(recv: &mut RecvStream) -> Result<Option<DataStreamHeader>> {
    read_framed_message(recv, "data header").await
}

async fn read_framed_message<T>(recv: &mut RecvStream, label: &str) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut prefix = [0_u8; 4];
    match recv.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(bytes_read)) => {
            bail!("peer closed {label} mid-frame after {bytes_read} bytes")
        }
        Err(ReadExactError::ReadError(ReadError::ClosedStream))
        | Err(ReadExactError::ReadError(ReadError::ConnectionLost(_))) => return Ok(None),
        Err(ReadExactError::ReadError(error)) => {
            return Err(error).with_context(|| format!("failed to read {label} prefix"))
        }
    }

    let payload_len = u32::from_be_bytes(prefix) as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&prefix);
    frame.resize(4 + payload_len, 0);
    recv.read_exact(&mut frame[4..])
        .await
        .with_context(|| format!("failed to read {label} payload"))?;
    Ok(Some(decode_frame(&frame)?))
}

fn stream_url(config: &SessionConfig) -> String {
    format!(
        "{}/v1/streams/{}",
        config.rendezvous_server.trim_end_matches('/'),
        config.stream_id
    )
}

fn next_message_id() -> String {
    let counter = MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("m{counter}")
}

fn short_fingerprint(raw: &str) -> Option<String> {
    let compact: String = raw.chars().take(16).collect();
    if compact.is_empty() {
        None
    } else {
        Some(compact)
    }
}

impl EventSink {
    fn from_config(config: &SessionConfig) -> Self {
        Self {
            stdio: config.stdio,
            json: config.json_events,
        }
    }

    fn emit_runtime_event(&self, event: RuntimeEvent) {
        if self.json {
            let value = match event {
                RuntimeEvent::StateChanged(state) => json!({
                    "event": "state_changed",
                    "state": format!("{state:?}"),
                }),
                RuntimeEvent::HandshakeCompleted { peer } => json!({
                    "event": "connected",
                    "session_id": peer.session_id,
                    "peer_name": peer.peer_name,
                    "peer_fingerprint": peer.peer_fingerprint,
                }),
                RuntimeEvent::ConnectionStatus { mode, remote_addr } => json!({
                    "event": "connection_status",
                    "mode": mode,
                    "remote_addr": remote_addr,
                }),
                RuntimeEvent::ChatSent { text } => json!({
                    "event": "chat_sent",
                    "text": text,
                }),
                RuntimeEvent::ChatReceived { text } => json!({
                    "event": "chat_received",
                    "text": text,
                }),
            };
            self.emit_json_event(value);
            return;
        }

        match event {
            RuntimeEvent::StateChanged(state) => eprintln!("state: {:?}", state),
            RuntimeEvent::HandshakeCompleted { peer } => eprintln!(
                "connected to {} ({}) session={}",
                peer.peer_name,
                peer.peer_fingerprint
                    .unwrap_or_else(|| "unknown".to_string()),
                peer.session_id
            ),
            RuntimeEvent::ConnectionStatus { mode, remote_addr } => eprintln!(
                "{} connection, remote={}",
                mode,
                remote_addr.unwrap_or_else(|| "unknown".to_string())
            ),
            RuntimeEvent::ChatSent { text } => eprintln!("sent: {}", text),
            RuntimeEvent::ChatReceived { text } => {
                if self.stdio {
                    eprintln!("received: {}", text);
                } else {
                    println!("{}", text);
                }
            }
        }
    }

    fn emit_json_event(&self, value: serde_json::Value) {
        eprintln!("{}", value);
    }
}

impl SessionConfig {
    fn from_args(role: Role, args: SessionArgs) -> Self {
        let stream_id = args
            .stream_id
            .or_else(|| std::env::var("SKYFFLA_STREAM_ID").ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                eprintln!("missing stream id: pass it as an argument or set SKYFFLA_STREAM_ID");
                std::process::exit(2);
            });
        if args.stdio && args.message.is_some() {
            eprintln!("--stdio cannot be combined with --message");
            std::process::exit(2);
        }
        Self {
            role,
            stream_id,
            rendezvous_server: args.server,
            download_dir: args.download_dir,
            peer_name: args.name.unwrap_or_else(default_peer_name),
            outgoing_message: args.message,
            stdio: args.stdio,
            json_events: args.json,
        }
    }
}

fn default_peer_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "skyffla-peer".into())
}

impl UiState {
    fn new(stream_id: &str, local_name: &str, peer_name: &str) -> Self {
        let history_path = history_file_path();
        let input_history = load_history(&history_path);
        let auto_accept_enabled = load_auto_accept();
        Self {
            stream_id: stream_id.to_string(),
            peer_name: peer_name.to_string(),
            local_name: local_name.to_string(),
            auto_accept_enabled,
            events: Vec::new(),
            transfers: Vec::new(),
            pending_offer: None,
            input_buffer: String::new(),
            cursor_index: 0,
            input_history,
            history_index: None,
            draft_buffer: None,
            history_path,
            rendered_event_lines: 0,
            rendered_width: None,
            next_event_row: 0,
        }
    }

    fn system(&mut self, message: String) {
        self.push_event(message);
    }

    fn chat(&mut self, speaker: &str, text: &str) {
        self.push_event(format!("{speaker}: {text}"));
    }

    fn upsert_transfer(&mut self, transfer: TransferUi) {
        if let Some(existing) = self.transfers.iter_mut().find(|t| t.id == transfer.id) {
            *existing = transfer;
        } else {
            self.transfers.push(transfer);
        }
    }

    fn mark_transfer_streaming(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Streaming;
        }
    }

    fn mark_transfer_completed(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Completed;
        }
    }

    fn mark_transfer_rejected(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Rejected;
        }
    }

    fn mark_transfer_cancelled(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Cancelled;
        }
    }

    fn cancellable_transfer_ids(&self) -> Vec<String> {
        self.transfers
            .iter()
            .filter(|transfer| {
                matches!(
                    transfer.state,
                    TransferStateUi::Pending
                        | TransferStateUi::AwaitingDecision
                        | TransferStateUi::Streaming
                )
            })
            .map(|transfer| transfer.id.clone())
            .collect()
    }

    fn has_transfer(&self, transfer_id: &str) -> bool {
        self.transfers
            .iter()
            .any(|transfer| transfer.id == transfer_id)
    }

    fn update_transfer_progress(&mut self, transfer_id: &str, done: u64, total: Option<u64>) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.bytes_done = done;
            if total.is_some() {
                transfer.bytes_total = total;
            }
            if transfer.state == TransferStateUi::Pending
                || transfer.state == TransferStateUi::AwaitingDecision
            {
                transfer.state = TransferStateUi::Streaming;
            }
        }
    }

    fn render(&mut self) {
        const SHOVEL_ART: &[&str] = &[
            r"   ===       skyffla.com",
            r"    |",
            r"    |        moving your bits, seamless and secure!",
            r"  __|__",
            r"  \   /",
            r"   \_/",
        ];
        let width = terminal_width();
        let height = terminal_height();
        let divider = "-".repeat(width);
        let prompt_row = height.saturating_sub(1) as u16;
        let (visible_input, cursor_col) = self.prompt_window(width);
        let mut stdout = std::io::stdout();

        if self.rendered_width != Some(width) {
            self.rendered_width = Some(width);
            self.rendered_event_lines = 0;
            let _ = queue!(stdout, MoveTo(0, 0), Clear(ClearType::All));
            let _ = queue!(
                stdout,
                MoveTo(0, 0),
                Clear(ClearType::CurrentLine),
                Print(clip_line(&divider, width))
            );
            for (index, line) in SHOVEL_ART.iter().enumerate() {
                let _ = queue!(
                    stdout,
                    MoveTo(0, index as u16 + 1),
                    Clear(ClearType::CurrentLine),
                    Print(clip_line(line, width))
                );
            }
            let _ = queue!(
                stdout,
                MoveTo(0, SHOVEL_ART.len() as u16 + 1),
                Clear(ClearType::CurrentLine),
                Print(clip_line(&divider, width))
            );
            let _ = write!(stdout, "\x1b[1;{}r", prompt_row);
            self.next_event_row = SHOVEL_ART.len() as u16 + 2;
        }

        let event_lines = if self.events.is_empty() {
            vec!["[--:--:--] waiting for events".to_string()]
        } else {
            self.render_event_lines(width)
        };
        for line in event_lines.iter().skip(self.rendered_event_lines) {
            if self.next_event_row >= prompt_row {
                let _ = queue!(stdout, ScrollUp(1));
                self.next_event_row = prompt_row.saturating_sub(1);
            }
            let _ = queue!(
                stdout,
                MoveTo(0, self.next_event_row),
                Clear(ClearType::CurrentLine),
                Print(clip_line(line, width))
            );
            self.next_event_row = self.next_event_row.saturating_add(1);
        }
        self.rendered_event_lines = event_lines.len();

        let _ = queue!(
            stdout,
            MoveTo(0, prompt_row),
            Clear(ClearType::CurrentLine),
            Print(format!("> {visible_input}")),
            MoveTo(cursor_col as u16, prompt_row),
            Show
        );
        let _ = stdout.flush();
    }

    fn render_event_lines(&self, width: usize) -> Vec<String> {
        self.events
            .iter()
            .flat_map(|event| {
                wrap_prefixed_lines(&format!("[{}] ", event.timestamp), &event.text, width)
            })
            .collect()
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Option<String> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("/quit".to_string());
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_index = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_index = self.input_buffer.len();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_buffer.truncate(self.cursor_index);
                self.history_index = None;
                self.draft_buffer = None;
            }
            KeyCode::Enter => {
                let submitted = self.input_buffer.trim().to_string();
                self.history_index = None;
                self.draft_buffer = None;
                self.cursor_index = 0;
                let previous = std::mem::take(&mut self.input_buffer);
                if !submitted.is_empty() {
                    self.record_history(previous);
                    return Some(submitted);
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_index = None;
                self.draft_buffer = None;
                self.input_buffer.insert(self.cursor_index, c);
                self.cursor_index += c.len_utf8();
            }
            KeyCode::Backspace => {
                if self.cursor_index > 0 {
                    let previous = previous_boundary(&self.input_buffer, self.cursor_index);
                    self.input_buffer.drain(previous..self.cursor_index);
                    self.cursor_index = previous;
                    self.history_index = None;
                    self.draft_buffer = None;
                }
            }
            KeyCode::Delete => {
                if self.cursor_index < self.input_buffer.len() {
                    let next = next_boundary(&self.input_buffer, self.cursor_index);
                    self.input_buffer.drain(self.cursor_index..next);
                    self.history_index = None;
                    self.draft_buffer = None;
                }
            }
            KeyCode::Left => {
                if self.cursor_index > 0 {
                    self.cursor_index = previous_boundary(&self.input_buffer, self.cursor_index);
                }
            }
            KeyCode::Right => {
                if self.cursor_index < self.input_buffer.len() {
                    self.cursor_index = next_boundary(&self.input_buffer, self.cursor_index);
                }
            }
            KeyCode::Up => self.history_up(),
            KeyCode::Down => self.history_down(),
            KeyCode::Home => self.cursor_index = 0,
            KeyCode::End => self.cursor_index = self.input_buffer.len(),
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.input_buffer.is_empty() =>
            {
                return Some("/quit".to_string());
            }
            _ => {}
        }

        None
    }

    fn push_event(&mut self, text: String) {
        self.events.push(EventLine {
            timestamp: compact_timestamp(),
            text,
        });
    }

    fn record_history(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        self.input_history.push(line);
        if self.input_history.len() > 500 {
            let overflow = self.input_history.len() - 500;
            self.input_history.drain(0..overflow);
        }
        save_history(&self.history_path, &self.input_history);
    }

    fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        let next_index = match self.history_index {
            Some(0) => 0,
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft_buffer = Some(self.input_buffer.clone());
                self.input_history.len() - 1
            }
        };
        self.history_index = Some(next_index);
        self.input_buffer = self.input_history[next_index].clone();
        self.cursor_index = self.input_buffer.len();
    }

    fn history_down(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.input_history.len() {
            let next_index = index + 1;
            self.history_index = Some(next_index);
            self.input_buffer = self.input_history[next_index].clone();
        } else {
            self.history_index = None;
            self.input_buffer = self.draft_buffer.take().unwrap_or_default();
        }
        self.cursor_index = self.input_buffer.len();
    }

    fn prompt_window(&self, width: usize) -> (String, usize) {
        let prompt_space = width.saturating_sub(2).max(1);
        let buffer_chars: Vec<char> = self.input_buffer.chars().collect();
        let cursor_chars = self.input_buffer[..self.cursor_index].chars().count();

        if buffer_chars.len() <= prompt_space {
            return (self.input_buffer.clone(), 2 + cursor_chars);
        }

        let mut start = cursor_chars.saturating_sub(prompt_space.saturating_sub(1));
        if start + prompt_space > buffer_chars.len() {
            start = buffer_chars.len().saturating_sub(prompt_space);
        }
        let end = (start + prompt_space).min(buffer_chars.len());
        let visible: String = buffer_chars[start..end].iter().collect();
        let cursor_col = 2 + cursor_chars.saturating_sub(start);
        (visible, cursor_col)
    }
}

fn parse_user_input(input: &str) -> UserInput {
    let trimmed = input.trim();
    match trimmed {
        "q" => return UserInput::Quit,
        "y" => return UserInput::Accept,
        "n" => return UserInput::Reject,
        _ => {}
    }
    if trimmed == "/quit" {
        UserInput::Quit
    } else if trimmed == "/help" {
        UserInput::Help
    } else if trimmed == "/clip" {
        UserInput::SendClipboard
    } else if trimmed == "/cancel" {
        UserInput::Cancel(None)
    } else if let Some(transfer_id) = trimmed.strip_prefix("/cancel ") {
        UserInput::Cancel(Some(transfer_id.trim().to_string()))
    } else if trimmed == "/autoaccept" {
        UserInput::AutoAccept(None)
    } else if trimmed == "/autoaccept on" {
        UserInput::AutoAccept(Some(true))
    } else if trimmed == "/autoaccept off" {
        UserInput::AutoAccept(Some(false))
    } else if trimmed == "/accept" {
        UserInput::Accept
    } else if trimmed == "/reject" {
        UserInput::Reject
    } else if let Some(path) = trimmed.strip_prefix("/send ") {
        UserInput::SendFile(expand_user_path(path.trim()))
    } else {
        UserInput::Chat(trimmed.to_string())
    }
}

fn help_lines() -> &'static [&'static str] {
    &[
        "commands:",
        "/help  show this help",
        "/send <path>  offer a file or folder",
        "/clip  offer clipboard text",
        "/accept  accept the pending file offer",
        "/reject  reject the pending file offer",
        "/cancel [id]  cancel an active transfer",
        "/autoaccept on|off  auto-accept incoming files and clipboard",
        "/quit  close the session",
        "shortcuts: q quit, y accept, n reject, ctrl+c close",
        "editing: up/down history, ctrl+a line start, ctrl+e line end, ctrl+k kill to end",
    ]
}

fn transfer_kind_label(kind: &TransferKind) -> &'static str {
    match kind {
        TransferKind::File => "file",
        TransferKind::FolderArchive => "folder",
        TransferKind::Clipboard => "clipboard",
        TransferKind::Stdio => "stdio",
    }
}

fn describe_offer_size(offer: &Offer) -> String {
    match (offer.kind.clone(), offer.size, offer.item_count) {
        (TransferKind::FolderArchive, Some(size), Some(items)) => {
            format!(" ({} items, {} bytes)", items, size)
        }
        (TransferKind::FolderArchive, _, Some(items)) => format!(" ({} items)", items),
        (_, Some(size), _) => format!(" ({} bytes)", size),
        _ => String::new(),
    }
}

fn history_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla_history"))
}

fn auto_accept_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".skyffla_autoaccept"))
}

fn load_history(path: &Option<PathBuf>) -> Vec<String> {
    let Some(path) = path else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn save_history(path: &Option<PathBuf>, history: &[String]) {
    let Some(path) = path else {
        return;
    };
    let contents = if history.is_empty() {
        String::new()
    } else {
        let mut joined = history.join("\n");
        joined.push('\n');
        joined
    };
    let _ = std::fs::write(path, contents);
}

fn load_auto_accept() -> bool {
    let Some(path) = auto_accept_file_path() else {
        return false;
    };
    match std::fs::read_to_string(path) {
        Ok(contents) => matches!(contents.trim(), "on" | "true" | "1"),
        Err(_) => false,
    }
}

fn save_auto_accept(enabled: bool) {
    let Some(path) = auto_accept_file_path() else {
        return;
    };
    let _ = std::fs::write(path, if enabled { "on\n" } else { "off\n" });
}

fn expand_user_path(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input));
    }

    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(input)
}

fn collect_folder_stats(path: &Path) -> Result<FolderStats> {
    fn visit(path: &Path, stats: &mut FolderStats) -> Result<()> {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
            let entry_path = entry.path();
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat {}", entry_path.display()))?;
            stats.item_count += 1;
            if metadata.is_dir() {
                visit(&entry_path, stats)?;
            } else if metadata.is_file() {
                stats.total_bytes += metadata.len();
            }
        }
        Ok(())
    }

    let mut stats = FolderStats {
        item_count: 0,
        total_bytes: 0,
    };
    visit(path, &mut stats)?;
    Ok(stats)
}

fn finalize_sha256_digest(hasher: Sha256) -> Digest {
    let bytes = hasher.finalize();
    Digest {
        algorithm: "sha256".to_string(),
        value_hex: format!("{bytes:x}"),
    }
}

fn format_digest_suffix(digest: Option<&Digest>) -> String {
    digest
        .map(|digest| format!(" [{}:{}]", digest.algorithm, digest.value_hex))
        .unwrap_or_default()
}

fn digest_json(digest: Option<&Digest>) -> serde_json::Value {
    match digest {
        Some(digest) => json!({
            "algorithm": digest.algorithm,
            "value_hex": digest.value_hex,
        }),
        None => serde_json::Value::Null,
    }
}

async fn read_clipboard_text() -> Result<String> {
    task::spawn_blocking(|| {
        let mut clipboard = Clipboard::new().context("failed to access local clipboard")?;
        clipboard
            .get_text()
            .context("failed to read text from local clipboard")
    })
    .await
    .context("clipboard read task failed")?
}

async fn write_clipboard_text(text: String) -> Result<()> {
    task::spawn_blocking(move || {
        let mut clipboard = Clipboard::new().context("failed to access local clipboard")?;
        clipboard
            .set_text(text)
            .context("failed to write text to local clipboard")
    })
    .await
    .context("clipboard write task failed")?
}

fn wrap_prefixed_lines(prefix: &str, text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![prefix.to_string()];
    }

    let available = width.saturating_sub(prefix.len()).max(8);
    let indent = " ".repeat(prefix.len());
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let next_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if next_len > available && !current.is_empty() {
            if lines.is_empty() {
                lines.push(format!("{prefix}{current}"));
            } else {
                lines.push(format!("{indent}{current}"));
            }
            current.clear();
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    if current.is_empty() {
        lines.push(prefix.to_string());
    } else if lines.is_empty() {
        lines.push(format!("{prefix}{current}"));
    } else {
        lines.push(format!("{indent}{current}"));
    }

    lines
}

fn terminal_width() -> usize {
    terminal_size()
        .map(|(cols, _)| cols as usize)
        .ok()
        .filter(|cols| *cols >= 20)
        .unwrap_or(72)
}

fn terminal_height() -> usize {
    terminal_size()
        .map(|(_, rows)| rows as usize)
        .ok()
        .filter(|rows| *rows >= 8)
        .unwrap_or(24)
}

fn previous_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| index + offset)
        .unwrap_or(text.len())
}

fn compact_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() % 86_400)
        .unwrap_or(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

fn clip_line(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}
