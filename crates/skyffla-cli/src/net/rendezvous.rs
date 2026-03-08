use anyhow::{Context, Result};
use reqwest::Client;
use skyffla_protocol::Capabilities;
use skyffla_rendezvous::{GetStreamResponse, PutStreamRequest, DEFAULT_TTL_SECONDS};
use skyffla_transport::PeerTicket;

use crate::config::SessionConfig;

pub(crate) async fn register_stream(
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

pub(crate) async fn resolve_stream(
    client: &Client,
    config: &SessionConfig,
) -> Result<GetStreamResponse> {
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

pub(crate) async fn delete_stream(client: &Client, config: &SessionConfig) -> Result<()> {
    client
        .delete(stream_url(config))
        .send()
        .await
        .context("failed to delete stream from rendezvous server")?
        .error_for_status()
        .context("rendezvous server rejected stream deletion")?;
    Ok(())
}

fn stream_url(config: &SessionConfig) -> String {
    format!(
        "{}/v1/streams/{}",
        config.rendezvous_server.trim_end_matches('/'),
        config.stream_id
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::{Role, SessionConfig};

    use super::stream_url;

    #[test]
    fn stream_url_trims_trailing_slash_from_server() {
        let config = SessionConfig {
            role: Role::Host,
            stream_id: "room".into(),
            rendezvous_server: "http://127.0.0.1:8080/".into(),
            download_dir: PathBuf::from("."),
            peer_name: "peer".into(),
            outgoing_message: None,
            stdio: false,
            json_events: false,
        };

        assert_eq!(stream_url(&config), "http://127.0.0.1:8080/v1/streams/room");
    }
}
