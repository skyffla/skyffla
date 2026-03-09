use std::fmt;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use skyffla_protocol::Capabilities;
use skyffla_rendezvous::{GetStreamResponse, PutStreamRequest, DEFAULT_TTL_SECONDS};
use skyffla_transport::PeerTicket;

use crate::config::SessionConfig;

pub(crate) async fn register_stream(
    client: &Client,
    config: &SessionConfig,
    ticket: &PeerTicket,
) -> std::result::Result<(), RegisterStreamError> {
    let response = client
        .put(stream_url(config))
        .json(&PutStreamRequest {
            ticket: ticket.encoded.clone(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        })
        .send()
        .await
        .context("failed to register stream with rendezvous server")
        .map_err(RegisterStreamError::Other)?;

    if response.status() == StatusCode::CONFLICT {
        let message = response
            .json::<RendezvousErrorResponse>()
            .await
            .ok()
            .map(|body| body.message)
            .unwrap_or_else(|| format!("stream {} is already hosted", config.stream_id));
        return Err(RegisterStreamError::AlreadyHosted {
            stream_id: config.stream_id.clone(),
            message,
        });
    }

    response
        .error_for_status()
        .context("rendezvous server rejected stream registration")
        .map_err(RegisterStreamError::Other)?;
    Ok(())
}

pub(crate) async fn resolve_stream(
    client: &Client,
    config: &SessionConfig,
) -> Result<Option<GetStreamResponse>> {
    let response = client
        .get(stream_url(config))
        .send()
        .await
        .context("failed to resolve stream from rendezvous server")?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let stream = response
        .error_for_status()
        .context("rendezvous server rejected stream lookup")?
        .json()
        .await
        .context("failed to decode rendezvous lookup response")?;
    Ok(Some(stream))
}

pub(crate) async fn delete_stream(client: &Client, config: &SessionConfig) -> Result<()> {
    let response = client
        .delete(stream_url(config))
        .send()
        .await
        .context("failed to delete stream from rendezvous server")?;

    if is_delete_success(response.status()) {
        return Ok(());
    }

    response
        .error_for_status()
        .context("rendezvous server rejected stream deletion")?;
    Ok(())
}

fn is_delete_success(status: StatusCode) -> bool {
    status.is_success() || status == StatusCode::NOT_FOUND
}

fn stream_url(config: &SessionConfig) -> String {
    format!(
        "{}/v1/streams/{}",
        config.rendezvous_server.trim_end_matches('/'),
        config.stream_id
    )
}

#[derive(Debug)]
pub(crate) enum RegisterStreamError {
    AlreadyHosted { stream_id: String, message: String },
    Other(anyhow::Error),
}

impl fmt::Display for RegisterStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHosted { message, .. } => message.fmt(f),
            Self::Other(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RegisterStreamError {}

#[derive(Debug, Deserialize)]
struct RendezvousErrorResponse {
    message: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::accept_policy::AutoAcceptPolicy;
    use crate::config::{Role, SessionConfig, DEFAULT_RENDEZVOUS_URL};

    use reqwest::StatusCode;

    use super::{is_delete_success, stream_url, RendezvousErrorResponse};

    #[test]
    fn stream_url_trims_trailing_slash_from_server() {
        let config = SessionConfig {
            role: Role::Host,
            stream_id: "room".into(),
            rendezvous_server: format!("{DEFAULT_RENDEZVOUS_URL}/"),
            download_dir: PathBuf::from("."),
            peer_name: "peer".into(),
            outgoing_message: None,
            stdio: false,
            json_events: false,
            local_mode: false,
            auto_accept_policy: AutoAcceptPolicy::none(),
            auto_accept_source: "default",
        };

        assert_eq!(
            stream_url(&config),
            format!("{DEFAULT_RENDEZVOUS_URL}/v1/streams/room")
        );
    }

    #[test]
    fn rendezvous_error_response_deserializes_message() {
        let body = serde_json::from_str::<RendezvousErrorResponse>(
            r#"{"error":"stream_already_exists","message":"stream demo is already hosted"}"#,
        )
        .expect("rendezvous error response should deserialize");

        assert_eq!(body.message, "stream demo is already hosted");
    }

    #[test]
    fn delete_treats_missing_stream_as_success() {
        assert!(is_delete_success(StatusCode::NO_CONTENT));
        assert!(is_delete_success(StatusCode::NOT_FOUND));
        assert!(!is_delete_success(StatusCode::INTERNAL_SERVER_ERROR));
    }
}
