use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use skyffla_session::SessionPeer;

use crate::local_state::{local_state_file_path, update_local_state, KnownPeerRecord, LocalState};

pub(crate) struct PeerTrustStatus {
    pub(crate) status: &'static str,
    pub(crate) peer_name: String,
    pub(crate) peer_fingerprint: Option<String>,
    pub(crate) previous_name: Option<String>,
}

pub(crate) fn remember_peer(peer: &SessionPeer) -> Result<Option<PeerTrustStatus>> {
    let now = unix_now();
    update_local_state(&local_state_file_path(), |state| {
        apply_peer_trust(state, peer, now)
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn apply_peer_trust(
    state: &mut LocalState,
    peer: &SessionPeer,
    now: u64,
) -> Option<PeerTrustStatus> {
    let Some(fingerprint) = peer.peer_fingerprint.clone() else {
        return Some(PeerTrustStatus {
            status: "unverified",
            peer_name: peer.peer_name.clone(),
            peer_fingerprint: None,
            previous_name: None,
        });
    };

    match state.known_peers.get_mut(&fingerprint) {
        Some(record) => {
            let previous_name = if record.peer_name != peer.peer_name {
                Some(record.peer_name.clone())
            } else {
                None
            };
            record.peer_name = peer.peer_name.clone();
            record.last_seen_unix = now;

            Some(PeerTrustStatus {
                status: if previous_name.is_some() {
                    "renamed"
                } else {
                    "known"
                },
                peer_name: peer.peer_name.clone(),
                peer_fingerprint: Some(fingerprint.clone()),
                previous_name: previous_name.clone(),
            })
        }
        None => {
            state.known_peers.insert(
                fingerprint.clone(),
                KnownPeerRecord {
                    peer_name: peer.peer_name.clone(),
                    first_seen_unix: now,
                    last_seen_unix: now,
                },
            );
            Some(PeerTrustStatus {
                status: "new",
                peer_name: peer.peer_name.clone(),
                peer_fingerprint: Some(fingerprint.clone()),
                previous_name: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use skyffla_session::SessionPeer;

    use super::apply_peer_trust;
    use crate::local_state::LocalState;

    #[test]
    fn new_peer_is_recorded_with_tofu_status() {
        let mut state = LocalState::default();
        let peer = SessionPeer {
            session_id: "room".into(),
            peer_name: "alice".into(),
            peer_fingerprint: Some("fingerprint".into()),
            peer_ticket: None,
            file_transfer_version: None,
        };

        let status = apply_peer_trust(&mut state, &peer, 123).expect("status");

        assert_eq!(status.status, "new");
        assert_eq!(state.known_peers["fingerprint"].peer_name, "alice");
        assert_eq!(state.known_peers["fingerprint"].first_seen_unix, 123);
    }

    #[test]
    fn known_peer_rename_is_reported_and_state_is_updated() {
        let mut state = LocalState::default();
        let original = SessionPeer {
            session_id: "room".into(),
            peer_name: "alice".into(),
            peer_fingerprint: Some("fingerprint".into()),
            peer_ticket: None,
            file_transfer_version: None,
        };
        let renamed = SessionPeer {
            session_id: "room".into(),
            peer_name: "alice-laptop".into(),
            peer_fingerprint: Some("fingerprint".into()),
            peer_ticket: None,
            file_transfer_version: None,
        };
        let _ = apply_peer_trust(&mut state, &original, 100);

        let status = apply_peer_trust(&mut state, &renamed, 200).expect("status");

        assert_eq!(status.status, "renamed");
        assert_eq!(status.previous_name.as_deref(), Some("alice"));
        assert_eq!(state.known_peers["fingerprint"].peer_name, "alice-laptop");
        assert_eq!(state.known_peers["fingerprint"].last_seen_unix, 200);
    }

    #[test]
    fn peer_without_fingerprint_is_unverified() {
        let mut state = LocalState::default();
        let peer = SessionPeer {
            session_id: "room".into(),
            peer_name: "anon".into(),
            peer_fingerprint: None,
            peer_ticket: None,
            file_transfer_version: None,
        };

        let status = apply_peer_trust(&mut state, &peer, 123).expect("status");

        assert_eq!(status.status, "unverified");
        assert!(state.known_peers.is_empty());
    }
}
