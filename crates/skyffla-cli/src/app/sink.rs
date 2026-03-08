use serde_json::json;
use skyffla_protocol::Digest;
use skyffla_session::RuntimeEvent;

use crate::config::SessionConfig;

#[derive(Clone, Copy)]
pub(crate) struct EventSink {
    stdio: bool,
    json: bool,
}

impl EventSink {
    pub(crate) fn from_config(config: &SessionConfig) -> Self {
        Self {
            stdio: config.stdio,
            json: config.json_events,
        }
    }

    pub(crate) fn emit_runtime_event(&self, event: RuntimeEvent) {
        if self.json {
            self.emit_json_event(runtime_event_json(event));
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

    pub(crate) fn emit_json_event(&self, value: serde_json::Value) {
        eprintln!("{}", value);
    }
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

fn runtime_event_json(event: RuntimeEvent) -> serde_json::Value {
    match event {
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
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use skyffla_protocol::Digest;
    use skyffla_session::{RuntimeEvent, SessionPeer};

    use super::{digest_json, runtime_event_json};

    #[test]
    fn digest_json_returns_null_when_absent() {
        assert_eq!(digest_json(None), serde_json::Value::Null);
    }

    #[test]
    fn digest_json_serializes_algorithm_and_hex_value() {
        let digest = Digest {
            algorithm: "sha256".into(),
            value_hex: "abcd".into(),
        };

        assert_eq!(
            digest_json(Some(&digest)),
            json!({
                "algorithm": "sha256",
                "value_hex": "abcd",
            })
        );
    }

    #[test]
    fn runtime_event_json_shapes_peer_trust_events() {
        let event = RuntimeEvent::PeerTrust {
            status: "known".into(),
            peer_name: "alice".into(),
            peer_fingerprint: Some("fp".into()),
            previous_name: Some("old".into()),
        };

        assert_eq!(
            runtime_event_json(event),
            json!({
                "event": "peer_trust",
                "status": "known",
                "peer_name": "alice",
                "peer_fingerprint": "fp",
                "previous_name": "old",
            })
        );
    }

    #[test]
    fn runtime_event_json_shapes_connected_events() {
        let event = RuntimeEvent::HandshakeCompleted {
            peer: SessionPeer {
                session_id: "room".into(),
                peer_name: "alice".into(),
                peer_fingerprint: Some("fp".into()),
            },
        };

        assert_eq!(
            runtime_event_json(event),
            json!({
                "event": "connected",
                "session_id": "room",
                "peer_name": "alice",
                "peer_fingerprint": "fp",
            })
        );
    }
}
