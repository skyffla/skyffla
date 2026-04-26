use crate::config::SessionConfig;
use crate::net::framing::{next_message_id, read_envelope, write_envelope};
use anyhow::{bail, Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_protocol::{
    Capabilities, ControlMessage, Envelope, Hello, HelloAck, TransportCapability,
    FILE_TRANSFER_PROTOCOL_VERSION, PIPE_STREAM_PROTOCOL_VERSION, WIRE_PROTOCOL_VERSION,
};
use skyffla_session::SessionPeer;

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
            protocol_version: WIRE_PROTOCOL_VERSION,
            file_transfer_version: Some(FILE_TRANSFER_PROTOCOL_VERSION),
            pipe_stream_version: Some(PIPE_STREAM_PROTOCOL_VERSION),
            session_id: session_id.to_string(),
            peer_name: config.peer_name.clone(),
            peer_fingerprint: local_fingerprint.map(ToOwned::to_owned),
            peer_ticket: local_ticket.map(ToOwned::to_owned),
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
            ensure_wire_protocol_compatible(hello.protocol_version, "protocol version mismatch")?;
            SessionPeer {
                session_id: hello.session_id,
                peer_name: hello.peer_name,
                peer_fingerprint: hello.peer_fingerprint,
                peer_ticket: hello.peer_ticket,
                file_transfer_version: hello.file_transfer_version,
                pipe_stream_version: hello.pipe_stream_version,
            }
        }
        other => bail!("expected hello from peer, got {:?}", other),
    };

    let ack = Envelope::new(
        session_id,
        next_message_id(),
        ControlMessage::HelloAck(HelloAck {
            protocol_version: WIRE_PROTOCOL_VERSION,
            session_id: session_id.to_string(),
        }),
    );
    write_envelope(send, &ack).await?;

    let peer_ack = read_envelope(recv)
        .await?
        .context("peer closed control stream before sending hello ack")?;
    match peer_ack.payload {
        ControlMessage::HelloAck(ack) => {
            ensure_wire_protocol_compatible(
                ack.protocol_version,
                "protocol version mismatch in hello ack",
            )?;
            Ok(peer)
        }
        other => bail!("expected hello ack from peer, got {:?}", other),
    }
}

fn ensure_wire_protocol_compatible(
    peer_version: skyffla_protocol::ProtocolVersion,
    context: &str,
) -> Result<()> {
    if peer_version.is_compatible_with(WIRE_PROTOCOL_VERSION) {
        return Ok(());
    }

    bail!(
        "{context}: local {}, peer {}",
        WIRE_PROTOCOL_VERSION,
        peer_version
    )
}

#[cfg(test)]
mod tests {
    use skyffla_protocol::ProtocolVersion;

    use super::ensure_wire_protocol_compatible;

    #[test]
    fn wire_handshake_allows_minor_differences_with_same_major() {
        let peer = ProtocolVersion::new(2, 7);
        assert!(ensure_wire_protocol_compatible(peer, "peer hello").is_ok());
    }

    #[test]
    fn wire_handshake_rejects_different_major_versions() {
        let peer = ProtocolVersion::new(3, 0);
        let error = ensure_wire_protocol_compatible(peer, "peer hello").unwrap_err();
        assert!(error.to_string().contains("local 2.1, peer 3.0"));
    }
}
