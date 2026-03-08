mod local_state;
mod session_runtime;
mod transfers;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use iroh::endpoint::{ReadError, ReadExactError, RecvStream, SendStream};
use local_state::{load_local_state, local_state_file_path, save_local_state, KnownPeerRecord};
use reqwest::Client;
use serde_json::json;
use session_runtime::run_connected_session;
use skyffla_protocol::{
    decode_frame, encode_frame, Capabilities, ControlMessage, DataStreamHeader, Digest, Envelope,
    ErrorMessage,
};
use skyffla_rendezvous::{GetStreamResponse, PutStreamRequest, DEFAULT_TTL_SECONDS};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{IrohTransport, PeerTicket};
use tokio::io::AsyncWriteExt;

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

struct PeerTrustStatus {
    status: &'static str,
    peer_name: String,
    peer_fingerprint: Option<String>,
    previous_name: Option<String>,
    message: String,
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

pub(crate) async fn send_transfer_error(
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

pub(crate) async fn write_envelope(send: &mut SendStream, envelope: &Envelope) -> Result<()> {
    let bytes = encode_frame(envelope)?;
    send.write_all(&bytes)
        .await
        .context("failed to write envelope bytes")?;
    send.flush()
        .await
        .context("failed to flush envelope bytes")?;
    Ok(())
}

pub(crate) async fn write_data_header(
    send: &mut SendStream,
    header: &DataStreamHeader,
) -> Result<()> {
    let bytes = encode_frame(header)?;
    send.write_all(&bytes)
        .await
        .context("failed to write data header bytes")?;
    send.flush()
        .await
        .context("failed to flush data header bytes")?;
    Ok(())
}

pub(crate) async fn read_envelope(recv: &mut RecvStream) -> Result<Option<Envelope>> {
    read_framed_message(recv, "envelope").await
}

pub(crate) async fn read_data_header(recv: &mut RecvStream) -> Result<Option<DataStreamHeader>> {
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

pub(crate) fn next_message_id() -> String {
    let counter = MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("m{counter}")
}

pub(crate) fn short_fingerprint(raw: &str) -> Option<String> {
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
                RuntimeEvent::PeerTrust {
                    status,
                    peer_name,
                    peer_fingerprint,
                    previous_name,
                } => json!({
                    "event": "peer_trust",
                    "status": status,
                    "peer_name": peer_name,
                    "peer_fingerprint": peer_fingerprint,
                    "previous_name": previous_name,
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
            RuntimeEvent::PeerTrust {
                status,
                peer_name,
                peer_fingerprint,
                previous_name,
            } => match status.as_str() {
                "new" => eprintln!(
                    "new peer {} ({})",
                    peer_name,
                    peer_fingerprint.unwrap_or_else(|| "unknown".to_string())
                ),
                "renamed" => eprintln!(
                    "known peer {} is now {} ({})",
                    previous_name.unwrap_or_else(|| "unknown".to_string()),
                    peer_name,
                    peer_fingerprint.unwrap_or_else(|| "unknown".to_string())
                ),
                "unverified" => eprintln!("peer {} is unverified", peer_name),
                _ => eprintln!(
                    "known peer {} ({})",
                    peer_name,
                    peer_fingerprint.unwrap_or_else(|| "unknown".to_string())
                ),
            },
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

pub(crate) fn remember_peer(peer: &SessionPeer) -> Option<PeerTrustStatus> {
    let Some(fingerprint) = peer.peer_fingerprint.clone() else {
        return Some(PeerTrustStatus {
            status: "unverified",
            peer_name: peer.peer_name.clone(),
            peer_fingerprint: None,
            previous_name: None,
            message: format!("peer {} is unverified", peer.peer_name),
        });
    };
    let now = unix_now();
    let mut state = load_local_state(&local_state_file_path());
    let known_peers = &mut state.known_peers;

    match known_peers.get_mut(&fingerprint) {
        Some(record) => {
            let previous_name = if record.peer_name != peer.peer_name {
                Some(record.peer_name.clone())
            } else {
                None
            };
            record.peer_name = peer.peer_name.clone();
            record.last_seen_unix = now;
            save_local_state(&local_state_file_path(), &state);

            Some(PeerTrustStatus {
                status: if previous_name.is_some() {
                    "renamed"
                } else {
                    "known"
                },
                peer_name: peer.peer_name.clone(),
                peer_fingerprint: Some(fingerprint.clone()),
                previous_name: previous_name.clone(),
                message: if let Some(previous_name) = previous_name {
                    format!(
                        "known peer {} is now {} ({})",
                        previous_name, peer.peer_name, fingerprint
                    )
                } else {
                    format!("known peer {} ({})", peer.peer_name, fingerprint)
                },
            })
        }
        None => {
            known_peers.insert(
                fingerprint.clone(),
                KnownPeerRecord {
                    peer_name: peer.peer_name.clone(),
                    first_seen_unix: now,
                    last_seen_unix: now,
                },
            );
            save_local_state(&local_state_file_path(), &state);
            Some(PeerTrustStatus {
                status: "new",
                peer_name: peer.peer_name.clone(),
                peer_fingerprint: Some(fingerprint.clone()),
                previous_name: None,
                message: format!(
                    "new peer trust-on-first-use: {} ({})",
                    peer.peer_name, fingerprint
                ),
            })
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn digest_json(digest: Option<&Digest>) -> serde_json::Value {
    match digest {
        Some(digest) => json!({
            "algorithm": digest.algorithm,
            "value_hex": digest.value_hex,
        }),
        None => serde_json::Value::Null,
    }
}
