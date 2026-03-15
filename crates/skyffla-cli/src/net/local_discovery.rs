use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use iroh::address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use iroh::endpoint_info::UserData;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use tokio::time::{timeout_at, Instant};

const LOCAL_DISCOVERY_SERVICE_NAME: &str = "skyffla-local-v1";
const LOCAL_DISCOVERY_PREFIX: &str = "skyffla-local-v1";
const JOIN_ELECTION_WINDOW: Duration = Duration::from_secs(3);
const HOST_ANNOUNCEMENT_GRACE: Duration = Duration::from_secs(3);
const JOIN_ELECTION_WINDOW_ENV: &str = "SKYFFLA_LOCAL_JOIN_ELECTION_WINDOW_MS";
const HOST_ANNOUNCEMENT_GRACE_ENV: &str = "SKYFFLA_LOCAL_HOST_ANNOUNCEMENT_GRACE_MS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalAnnouncement {
    Host,
    Candidate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalJoinDecision {
    Connect(EndpointAddr),
    Host,
}

pub(crate) fn enable_local_discovery(
    endpoint: &Endpoint,
    stream_id: &str,
    announcement: LocalAnnouncement,
) -> Result<MdnsAddressLookup> {
    let mdns = MdnsAddressLookup::builder()
        .service_name(LOCAL_DISCOVERY_SERVICE_NAME)
        .build(endpoint.id())
        .context("failed to enable local network discovery")?;
    endpoint.address_lookup().add(mdns.clone());
    set_local_announcement(endpoint, stream_id, announcement)?;
    Ok(mdns)
}

pub(crate) fn set_local_announcement(
    endpoint: &Endpoint,
    stream_id: &str,
    announcement: LocalAnnouncement,
) -> Result<()> {
    let tag = announcement_tag(stream_id, announcement);
    let user_data =
        UserData::try_from(tag).context("failed to encode local discovery announcement")?;
    endpoint.set_user_data_for_address_lookup(Some(user_data));
    Ok(())
}

pub(crate) async fn resolve_local_join_decision(
    mdns: &MdnsAddressLookup,
    stream_id: &str,
    local_id: EndpointId,
) -> Result<LocalJoinDecision> {
    let timing = local_discovery_timing();
    let mut events = mdns.subscribe().await;
    let mut candidate_endpoints = BTreeMap::from([(local_id, None)]);
    let election_deadline = Instant::now() + timing.join_election_window;

    loop {
        match timeout_at(election_deadline, events.next()).await {
            Ok(Some(event)) => {
                if let Some(match_event) = match_stream_event(event, stream_id) {
                    match match_event {
                        MatchedStreamEvent::Host(endpoint_addr) => {
                            return Ok(LocalJoinDecision::Connect(endpoint_addr));
                        }
                        MatchedStreamEvent::Candidate(endpoint_id, endpoint_addr) => {
                            candidate_endpoints.insert(endpoint_id, Some(endpoint_addr));
                        }
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let host_deadline = Instant::now() + timing.host_announcement_grace;
    loop {
        match timeout_at(host_deadline, events.next()).await {
            Ok(Some(event)) => {
                if let Some(match_event) = match_stream_event(event, stream_id) {
                    match match_event {
                        MatchedStreamEvent::Host(endpoint_addr) => {
                            return Ok(LocalJoinDecision::Connect(endpoint_addr));
                        }
                        MatchedStreamEvent::Candidate(endpoint_id, endpoint_addr) => {
                            candidate_endpoints.insert(endpoint_id, Some(endpoint_addr));
                        }
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let candidate_ids = candidate_endpoints.keys().copied().collect::<BTreeSet<_>>();
    let winner = elected_candidate_endpoint(local_id, &candidate_endpoints);

    if should_promote_to_host(local_id, &candidate_ids) {
        return Ok(LocalJoinDecision::Host);
    }

    winner.map(LocalJoinDecision::Connect).ok_or_else(|| {
        anyhow::anyhow!(
            "local join election chose a remote host but no candidate endpoint was available"
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalDiscoveryTiming {
    join_election_window: Duration,
    host_announcement_grace: Duration,
}

fn local_discovery_timing() -> LocalDiscoveryTiming {
    LocalDiscoveryTiming {
        join_election_window: duration_from_env(JOIN_ELECTION_WINDOW_ENV, JOIN_ELECTION_WINDOW),
        host_announcement_grace: duration_from_env(
            HOST_ANNOUNCEMENT_GRACE_ENV,
            HOST_ANNOUNCEMENT_GRACE,
        ),
    }
}

fn duration_from_env(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|duration| !duration.is_zero())
        .unwrap_or(default)
}

fn announcement_tag(stream_id: &str, announcement: LocalAnnouncement) -> String {
    let role = match announcement {
        LocalAnnouncement::Host => "host",
        LocalAnnouncement::Candidate => "candidate",
    };
    format!("{LOCAL_DISCOVERY_PREFIX}:{role}:{stream_id}")
}

fn match_stream_event(event: DiscoveryEvent, stream_id: &str) -> Option<MatchedStreamEvent> {
    match event {
        DiscoveryEvent::Discovered { endpoint_info, .. } => {
            let announcement = parse_announcement(endpoint_info.data.user_data()?, stream_id)?;
            match announcement {
                LocalAnnouncement::Host => {
                    Some(MatchedStreamEvent::Host(endpoint_info.to_endpoint_addr()))
                }
                LocalAnnouncement::Candidate => Some(MatchedStreamEvent::Candidate(
                    endpoint_info.endpoint_id,
                    endpoint_info.to_endpoint_addr(),
                )),
            }
        }
        DiscoveryEvent::Expired { .. } => None,
    }
}

fn parse_announcement(user_data: &UserData, stream_id: &str) -> Option<LocalAnnouncement> {
    match user_data.as_ref() {
        value if value == announcement_tag(stream_id, LocalAnnouncement::Host) => {
            Some(LocalAnnouncement::Host)
        }
        value if value == announcement_tag(stream_id, LocalAnnouncement::Candidate) => {
            Some(LocalAnnouncement::Candidate)
        }
        _ => None,
    }
}

fn should_promote_to_host(local_id: EndpointId, candidate_ids: &BTreeSet<EndpointId>) -> bool {
    candidate_ids
        .iter()
        .next()
        .copied()
        .is_some_and(|winner| winner == local_id)
}

fn elected_candidate_endpoint(
    local_id: EndpointId,
    candidate_endpoints: &BTreeMap<EndpointId, Option<EndpointAddr>>,
) -> Option<EndpointAddr> {
    let candidate_ids = candidate_endpoints.keys().copied().collect::<BTreeSet<_>>();
    if should_promote_to_host(local_id, &candidate_ids) {
        return None;
    }

    candidate_endpoints
        .iter()
        .find(|(endpoint_id, _)| **endpoint_id != local_id)
        .and_then(|(_, endpoint_addr)| endpoint_addr.clone())
}

enum MatchedStreamEvent {
    Host(EndpointAddr),
    Candidate(EndpointId, EndpointAddr),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::str::FromStr;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use super::{
        announcement_tag, duration_from_env, elected_candidate_endpoint, local_discovery_timing,
        parse_announcement, should_promote_to_host, LocalAnnouncement, HOST_ANNOUNCEMENT_GRACE_ENV,
        JOIN_ELECTION_WINDOW_ENV,
    };
    use std::collections::BTreeMap;

    use iroh::endpoint_info::UserData;
    use iroh::EndpointId;
    use skyffla_transport::{IrohTransport, TransportError};

    use crate::net::local_discovery::{HOST_ANNOUNCEMENT_GRACE, JOIN_ELECTION_WINDOW};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn parses_stream_host_and_candidate_announcements() {
        let host = UserData::from_str(&announcement_tag("room", LocalAnnouncement::Host)).unwrap();
        let candidate =
            UserData::from_str(&announcement_tag("room", LocalAnnouncement::Candidate)).unwrap();

        assert_eq!(
            parse_announcement(&host, "room"),
            Some(LocalAnnouncement::Host)
        );
        assert_eq!(
            parse_announcement(&candidate, "room"),
            Some(LocalAnnouncement::Candidate)
        );
        assert_eq!(parse_announcement(&host, "other-room"), None);
    }

    #[test]
    fn timing_uses_defaults_when_overrides_are_missing_or_invalid() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var(JOIN_ELECTION_WINDOW_ENV);
            std::env::remove_var(HOST_ANNOUNCEMENT_GRACE_ENV);
        }
        let timing = local_discovery_timing();
        assert_eq!(timing.join_election_window, JOIN_ELECTION_WINDOW);
        assert_eq!(timing.host_announcement_grace, HOST_ANNOUNCEMENT_GRACE);

        unsafe {
            std::env::set_var(JOIN_ELECTION_WINDOW_ENV, "nope");
            std::env::set_var(HOST_ANNOUNCEMENT_GRACE_ENV, "0");
        }
        let timing = local_discovery_timing();
        assert_eq!(timing.join_election_window, JOIN_ELECTION_WINDOW);
        assert_eq!(timing.host_announcement_grace, HOST_ANNOUNCEMENT_GRACE);
    }

    #[test]
    fn timing_uses_millisecond_overrides() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var(JOIN_ELECTION_WINDOW_ENV, "125");
            std::env::set_var(HOST_ANNOUNCEMENT_GRACE_ENV, "250");
        }
        let timing = local_discovery_timing();
        assert_eq!(timing.join_election_window, Duration::from_millis(125));
        assert_eq!(timing.host_announcement_grace, Duration::from_millis(250));
        assert_eq!(
            duration_from_env(JOIN_ELECTION_WINDOW_ENV, Duration::from_secs(1)),
            Duration::from_millis(125)
        );
    }

    #[tokio::test]
    async fn lowest_candidate_endpoint_id_becomes_host() {
        let Some(first_transport) = bind_or_skip().await else {
            return;
        };
        let Some(second_transport) = bind_or_skip().await else {
            first_transport.close().await;
            return;
        };
        let first: EndpointId = first_transport.endpoint().id();
        let second: EndpointId = second_transport.endpoint().id();
        let (low, high) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };
        let candidates = BTreeSet::from([high, low]);

        assert!(should_promote_to_host(low, &candidates));
        assert!(!should_promote_to_host(high, &candidates));

        first_transport.close().await;
        second_transport.close().await;
    }

    #[tokio::test]
    async fn remote_winner_endpoint_becomes_fallback_connect_target() {
        let Some(first_transport) = bind_or_skip().await else {
            return;
        };
        let Some(second_transport) = bind_or_skip().await else {
            first_transport.close().await;
            return;
        };
        let first: EndpointId = first_transport.endpoint().id();
        let second: EndpointId = second_transport.endpoint().id();
        let (local, remote, remote_addr) = if first > second {
            (first, second, second_transport.endpoint().addr())
        } else {
            (second, first, first_transport.endpoint().addr())
        };
        let candidate_endpoints =
            BTreeMap::from([(local, None), (remote, Some(remote_addr.clone()))]);

        assert_eq!(
            elected_candidate_endpoint(local, &candidate_endpoints),
            Some(remote_addr)
        );

        first_transport.close().await;
        second_transport.close().await;
    }

    async fn bind_or_skip() -> Option<IrohTransport> {
        match IrohTransport::bind().await {
            Ok(transport) => Some(transport),
            Err(error) if is_socket_permission_error(&error) => None,
            Err(error) => panic!("bind should succeed: {error}"),
        }
    }

    fn is_socket_permission_error(error: &TransportError) -> bool {
        matches!(error, TransportError::EndpointBind(bind_error) if {
            let message = bind_error.to_string();
            message.contains("Operation not permitted") || message.contains("Failed to bind sockets")
        })
    }
}
