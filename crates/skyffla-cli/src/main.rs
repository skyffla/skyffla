use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use iroh::endpoint::{ReadError, ReadExactError, RecvStream, SendStream};
use reqwest::Client;
use skyffla_protocol::{
    decode_frame, encode_frame, Capabilities, ChatMessage, ControlMessage, Envelope, Hello,
    HelloAck, TransportCapability, PROTOCOL_VERSION,
};
use skyffla_rendezvous::{GetStreamResponse, PutStreamRequest, DEFAULT_TTL_SECONDS};
use skyffla_session::{
    state_changed_event, RuntimeEvent, SessionEvent, SessionMachine, SessionPeer,
};
use skyffla_transport::{IrohConnection, IrohTransport, PeerTicket};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
    stream_id: String,
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    server: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    message: Option<String>,
}

#[derive(Clone, Copy)]
enum Role {
    Host,
    Join,
}

struct SessionConfig {
    stream_id: String,
    rendezvous_server: String,
    peer_name: String,
    outgoing_message: Option<String>,
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
    let client = Client::new();
    let mut session = SessionMachine::new();
    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::HostRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter hosting state")?,
    ));

    let transport = IrohTransport::bind().await?;
    let ticket = transport.local_ticket()?;
    register_stream(&client, &config, &ticket).await?;
    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")?,
    ));

    eprintln!(
        "waiting for peer on stream {} via {}",
        config.stream_id, config.rendezvous_server
    );
    let connection = transport.accept_connection().await?;
    let result = run_connected_session(&config, &mut session, &transport, connection, true).await;
    let delete_result = delete_stream(&client, &config).await;
    transport.close().await;
    delete_result?;
    result
}

async fn run_join(args: SessionArgs) -> Result<()> {
    let config = SessionConfig::from_args(Role::Join, args);
    let client = Client::new();
    let mut session = SessionMachine::new();
    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::JoinRequested {
                stream_id: config.stream_id.clone(),
            })
            .context("failed to enter joining state")?,
    ));

    let transport = IrohTransport::bind().await?;
    let peer = resolve_stream(&client, &config).await?;
    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::TransportConnecting)
            .context("failed to enter connecting state")?,
    ));
    let connection = transport
        .connect(&PeerTicket {
            encoded: peer.ticket,
        })
        .await?;
    let result = run_connected_session(&config, &mut session, &transport, connection, false).await;
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
    session: &mut SessionMachine,
    transport: &IrohTransport,
    connection: IrohConnection,
    is_host: bool,
) -> Result<()> {
    let session_id = config.stream_id.clone();
    emit_runtime_event(state_changed_event(
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

    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Negotiated {
                session_id: session_id.clone(),
                stdio: false,
            })
            .context("failed to record negotiated state")?,
    ));
    emit_runtime_event(RuntimeEvent::HandshakeCompleted { peer });

    if let Some(message) = &config.outgoing_message {
        send_chat_message(&session_id, &mut send, message).await?;
        emit_runtime_event(RuntimeEvent::ChatSent {
            text: message.clone(),
        });
        send.finish()
            .context("failed to finish control stream send side")?;

        while let Some(envelope) = read_envelope(&mut recv).await? {
            handle_post_handshake_message(envelope)?;
        }
    } else {
        eprintln!("interactive chat ready; type /quit to exit");
        run_interactive_chat_loop(&session_id, &mut send, &mut recv).await?;
    }

    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::CloseRequested)
            .context("failed to enter closing state")?,
    ));
    emit_runtime_event(state_changed_event(
        session
            .transition(SessionEvent::Closed)
            .context("failed to enter closed state")?,
    ));

    Ok(())
}

async fn run_interactive_chat_loop(
    session_id: &str,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut send_open = true;

    loop {
        if !send_open {
            match read_envelope(recv).await? {
                Some(envelope) => handle_post_handshake_message(envelope)?,
                None => break,
            }
            continue;
        }

        tokio::select! {
            line = lines.next_line() => {
                match line.context("failed to read stdin line")? {
                    Some(line) => {
                        let text = line.trim_end().to_string();
                        if text == "/quit" {
                            send.finish().context("failed to finish control stream send side")?;
                            send_open = false;
                        } else if !text.is_empty() {
                            send_chat_message(session_id, send, &text).await?;
                            emit_runtime_event(RuntimeEvent::ChatSent { text });
                        }
                    }
                    None => {
                        send.finish().context("failed to finish control stream send side")?;
                        send_open = false;
                    }
                }
            }
            envelope = read_envelope(recv) => {
                match envelope? {
                    Some(envelope) => handle_post_handshake_message(envelope)?,
                    None => break,
                }
            }
        }
    }

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

async fn send_chat_message(session_id: &str, send: &mut SendStream, text: &str) -> Result<()> {
    let envelope = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::ChatMessage(ChatMessage {
            text: text.to_string(),
        }),
    );
    write_envelope(send, &envelope).await
}

fn handle_post_handshake_message(envelope: Envelope) -> Result<()> {
    match envelope.payload {
        ControlMessage::ChatMessage(message) => {
            emit_runtime_event(RuntimeEvent::ChatReceived { text: message.text });
            Ok(())
        }
        other => bail!("unexpected control message after handshake: {:?}", other),
    }
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

async fn read_envelope(recv: &mut RecvStream) -> Result<Option<Envelope>> {
    let mut prefix = [0_u8; 4];
    match recv.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(bytes_read)) => {
            bail!("peer closed control stream mid-frame after {bytes_read} bytes")
        }
        Err(ReadExactError::ReadError(ReadError::ClosedStream))
        | Err(ReadExactError::ReadError(ReadError::ConnectionLost(_))) => return Ok(None),
        Err(ReadExactError::ReadError(error)) => {
            return Err(error).context("failed to read envelope prefix")
        }
    }

    let payload_len = u32::from_be_bytes(prefix) as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&prefix);
    frame.resize(4 + payload_len, 0);
    recv.read_exact(&mut frame[4..])
        .await
        .context("failed to read envelope payload")?;
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

fn emit_runtime_event(event: RuntimeEvent) {
    match event {
        RuntimeEvent::StateChanged(state) => eprintln!("state: {:?}", state),
        RuntimeEvent::HandshakeCompleted { peer } => eprintln!(
            "connected to {} ({}) session={}",
            peer.peer_name,
            peer.peer_fingerprint
                .unwrap_or_else(|| "unknown".to_string()),
            peer.session_id
        ),
        RuntimeEvent::ChatSent { text } => eprintln!("sent: {}", text),
        RuntimeEvent::ChatReceived { text } => println!("{}", text),
    }
}

impl SessionConfig {
    fn from_args(_role: Role, args: SessionArgs) -> Self {
        Self {
            stream_id: args.stream_id,
            rendezvous_server: args.server,
            peer_name: args.name.unwrap_or_else(default_peer_name),
            outgoing_message: args.message,
        }
    }
}

fn default_peer_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "skyffla-peer".into())
}
