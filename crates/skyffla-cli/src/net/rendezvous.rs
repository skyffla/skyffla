use std::fmt;

use anyhow::{Context, Result};
use reqwest::{header::HeaderMap, Client, StatusCode};
use serde::Deserialize;
use skyffla_protocol::{Capabilities, ProtocolVersion};
use skyffla_rendezvous::{
    GetRoomResponse, PutRoomRequest, DEFAULT_TTL_SECONDS, RENDEZVOUS_API_VERSION,
    RENDEZVOUS_VERSION_HEADER,
};
use skyffla_transport::PeerTicket;

use crate::config::SessionConfig;

pub(crate) async fn register_room(
    client: &Client,
    config: &SessionConfig,
    ticket: &PeerTicket,
) -> std::result::Result<(), RegisterRoomError> {
    let response = client
        .put(room_url(config))
        .json(&PutRoomRequest {
            ticket: ticket.encoded.clone(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        })
        .send()
        .await
        .context("failed to register room with rendezvous server")
        .map_err(RegisterRoomError::Other)?;
    ensure_rendezvous_compatible(response.headers()).map_err(RegisterRoomError::Other)?;

    if response.status() == StatusCode::CONFLICT {
        let message = response
            .json::<RendezvousErrorResponse>()
            .await
            .ok()
            .map(|body| body.message)
            .unwrap_or_else(|| format!("room {} is already hosted", config.stream_id));
        return Err(RegisterRoomError::AlreadyHosted {
            room_id: config.stream_id.clone(),
            message,
        });
    }

    response
        .error_for_status()
        .context("rendezvous server rejected room registration")
        .map_err(RegisterRoomError::Other)?;
    Ok(())
}

pub(crate) async fn resolve_room(
    client: &Client,
    config: &SessionConfig,
) -> Result<Option<GetRoomResponse>> {
    let response = client
        .get(room_url(config))
        .send()
        .await
        .context("failed to resolve room from rendezvous server")?;
    ensure_rendezvous_compatible(response.headers())?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let room = response
        .error_for_status()
        .context("rendezvous server rejected room lookup")?
        .json()
        .await
        .context("failed to decode rendezvous lookup response")?;
    Ok(Some(room))
}

pub(crate) async fn delete_room(client: &Client, config: &SessionConfig) -> Result<()> {
    let response = client
        .delete(room_url(config))
        .send()
        .await
        .context("failed to delete room from rendezvous server")?;
    ensure_rendezvous_compatible(response.headers())?;

    if is_delete_success(response.status()) {
        return Ok(());
    }

    response
        .error_for_status()
        .context("rendezvous server rejected room deletion")?;
    Ok(())
}

fn ensure_rendezvous_compatible(headers: &HeaderMap) -> Result<()> {
    let raw = headers
        .get(RENDEZVOUS_VERSION_HEADER)
        .context("rendezvous server did not send a protocol version header")?
        .to_str()
        .context("rendezvous server sent a non-utf8 protocol version header")?;
    let version = parse_rendezvous_version(raw)?;
    if !version.is_compatible_with(RENDEZVOUS_API_VERSION) {
        anyhow::bail!(
            "rendezvous api version mismatch: local {}, server {}",
            RENDEZVOUS_API_VERSION,
            version
        );
    }
    Ok(())
}

fn parse_rendezvous_version(value: &str) -> Result<ProtocolVersion> {
    let (major, minor) = value
        .split_once('.')
        .context("rendezvous protocol version must be in <major>.<minor> form")?;
    Ok(ProtocolVersion::new(
        major.parse().context("invalid rendezvous major version")?,
        minor.parse().context("invalid rendezvous minor version")?,
    ))
}

fn is_delete_success(status: StatusCode) -> bool {
    status.is_success() || status == StatusCode::NOT_FOUND
}

fn room_url(config: &SessionConfig) -> String {
    format!(
        "{}/v1/rooms/{}",
        config.rendezvous_server.trim_end_matches('/'),
        config.stream_id
    )
}

#[derive(Debug)]
pub(crate) enum RegisterRoomError {
    AlreadyHosted { room_id: String, message: String },
    Other(anyhow::Error),
}

impl fmt::Display for RegisterRoomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHosted { message, .. } => message.fmt(f),
            Self::Other(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RegisterRoomError {}

#[derive(Debug, Deserialize)]
struct RendezvousErrorResponse {
    message: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::{Role, SessionConfig, DEFAULT_RENDEZVOUS_URL};

    use reqwest::StatusCode;

    use super::{
        ensure_rendezvous_compatible, is_delete_success, parse_rendezvous_version, room_url,
        RendezvousErrorResponse,
    };
    use reqwest::header::{HeaderMap, HeaderValue};
    use skyffla_rendezvous::{RENDEZVOUS_API_VERSION, RENDEZVOUS_VERSION_HEADER};

    #[test]
    fn room_url_trims_trailing_slash_from_server() {
        let config = SessionConfig {
            role: Role::Host,
            stream_id: "room".into(),
            rendezvous_server: format!("{DEFAULT_RENDEZVOUS_URL}/"),
            download_dir: PathBuf::from("."),
            peer_name: "peer".into(),
            outgoing_message: None,
            stdio: false,
            machine: false,
            json_events: false,
            local_mode: false,
        };

        assert_eq!(
            room_url(&config),
            format!("{DEFAULT_RENDEZVOUS_URL}/v1/rooms/room")
        );
    }

    #[test]
    fn rendezvous_error_response_deserializes_message() {
        let body = serde_json::from_str::<RendezvousErrorResponse>(
            r#"{"error":"room_already_exists","message":"room demo is already hosted"}"#,
        )
        .expect("rendezvous error response should deserialize");

        assert_eq!(body.message, "room demo is already hosted");
    }

    #[test]
    fn delete_treats_missing_room_as_success() {
        assert!(is_delete_success(StatusCode::NO_CONTENT));
        assert!(is_delete_success(StatusCode::NOT_FOUND));
        assert!(!is_delete_success(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn parses_rendezvous_version_header() {
        let parsed = parse_rendezvous_version("1.7").unwrap();
        assert_eq!(parsed.major, 1);
        assert_eq!(parsed.minor, 7);
    }

    #[test]
    fn rejects_incompatible_rendezvous_major_version() {
        let mut headers = HeaderMap::new();
        headers.insert(
            RENDEZVOUS_VERSION_HEADER,
            HeaderValue::from_static("2.0"),
        );
        let error = ensure_rendezvous_compatible(&headers).unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("local {}, server 2.0", RENDEZVOUS_API_VERSION)));
    }
}
