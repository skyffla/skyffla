use std::fmt;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use skyffla_protocol::Capabilities;
use skyffla_rendezvous::{GetRoomResponse, PutRoomRequest, DEFAULT_TTL_SECONDS};
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

    if is_delete_success(response.status()) {
        return Ok(());
    }

    response
        .error_for_status()
        .context("rendezvous server rejected room deletion")?;
    Ok(())
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

    use super::{is_delete_success, room_url, RendezvousErrorResponse};

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
}
