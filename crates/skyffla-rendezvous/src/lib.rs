pub mod app;
pub mod store;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use skyffla_protocol::{Capabilities, ProtocolVersion};

pub const DEFAULT_TTL_SECONDS: u64 = 600;
pub const MAX_TTL_SECONDS: u64 = 3600;
pub const RENDEZVOUS_API_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);
pub const RENDEZVOUS_VERSION_HEADER: &str = "x-skyffla-rendezvous-version";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutRoomRequest {
    pub ticket: String,
    pub ttl_seconds: u64,
    pub capabilities: Capabilities,
}

impl PutRoomRequest {
    pub fn normalized_ttl_seconds(&self) -> Result<u64, RegistryError> {
        if self.ttl_seconds == 0 {
            return Err(RegistryError::InvalidTtl {
                ttl_seconds: self.ttl_seconds,
            });
        }

        Ok(self.ttl_seconds.min(MAX_TTL_SECONDS))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutRoomResponse {
    pub room_id: String,
    pub expires_at_epoch_seconds: u64,
    pub status: RoomStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetRoomResponse {
    pub room_id: String,
    pub ticket: String,
    pub expires_at_epoch_seconds: u64,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomStatus {
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRecord {
    pub room_id: String,
    pub ticket: String,
    pub capabilities: Capabilities,
    pub expires_at_epoch_seconds: u64,
}

impl RoomRecord {
    pub fn is_expired_at(&self, now_epoch_seconds: u64) -> bool {
        self.expires_at_epoch_seconds <= now_epoch_seconds
    }

    pub fn as_get_response(&self) -> GetRoomResponse {
        GetRoomResponse {
            room_id: self.room_id.clone(),
            ticket: self.ticket.clone(),
            expires_at_epoch_seconds: self.expires_at_epoch_seconds,
            capabilities: self.capabilities.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct RoomRegistry {
    entries: BTreeMap<String, RoomRecord>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(
        &mut self,
        room_id: impl Into<String>,
        request: PutRoomRequest,
        now_epoch_seconds: u64,
    ) -> Result<PutRoomResponse, RegistryError> {
        let room_id = room_id.into();
        self.purge_expired(now_epoch_seconds);

        let ttl_seconds = request.normalized_ttl_seconds()?;
        let expires_at_epoch_seconds = now_epoch_seconds + ttl_seconds;
        if let Some(existing) = self.entries.get_mut(&room_id) {
            if existing.ticket != request.ticket {
                return Err(RegistryError::RoomAlreadyExists {
                    room_id: room_id.clone(),
                });
            }

            existing.capabilities = request.capabilities;
            existing.expires_at_epoch_seconds = expires_at_epoch_seconds;
        } else {
            let record = RoomRecord {
                room_id: room_id.clone(),
                ticket: request.ticket,
                capabilities: request.capabilities,
                expires_at_epoch_seconds,
            };
            self.entries.insert(room_id.clone(), record);
        }

        Ok(PutRoomResponse {
            room_id,
            expires_at_epoch_seconds,
            status: RoomStatus::Waiting,
        })
    }

    pub fn get(
        &mut self,
        room_id: &str,
        now_epoch_seconds: u64,
    ) -> Result<GetRoomResponse, RegistryError> {
        self.purge_expired(now_epoch_seconds);

        self.entries
            .get(room_id)
            .map(RoomRecord::as_get_response)
            .ok_or_else(|| RegistryError::RoomNotFound {
                room_id: room_id.to_string(),
            })
    }

    pub fn delete(&mut self, room_id: &str) -> bool {
        self.entries.remove(room_id).is_some()
    }

    pub fn purge_expired(&mut self, now_epoch_seconds: u64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, record| !record.is_expired_at(now_epoch_seconds));
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    InvalidTtl { ttl_seconds: u64 },
    RoomAlreadyExists { room_id: String },
    RoomNotFound { room_id: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTtl { ttl_seconds } => {
                write!(
                    f,
                    "ttl_seconds must be greater than zero, got {ttl_seconds}"
                )
            }
            Self::RoomAlreadyExists { room_id } => {
                write!(f, "room {room_id} is already hosted")
            }
            Self::RoomNotFound { room_id } => write!(f, "room {room_id} was not found"),
        }
    }
}

impl std::error::Error for RegistryError {}

pub fn unix_timestamp_seconds() -> Result<u64, std::time::SystemTimeError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> PutRoomRequest {
        PutRoomRequest {
            ticket: "bootstrap-ticket".into(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn put_and_get_room_round_trip() {
        let mut registry = RoomRegistry::new();
        let response = registry
            .put("demo-room", sample_request(), 1_000)
            .expect("put should succeed");

        assert_eq!(response.room_id, "demo-room");
        assert_eq!(response.status, RoomStatus::Waiting);

        let resolved = registry
            .get("demo-room", 1_001)
            .expect("get should resolve existing room");
        assert_eq!(resolved.ticket, "bootstrap-ticket");
        assert_eq!(resolved.capabilities, Capabilities::default());
    }

    #[test]
    fn registry_rejects_duplicate_live_room() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut conflicting = sample_request();
        conflicting.ticket = "different-ticket".into();
        let result = registry.put("demo-room", conflicting, 1_100);
        assert_eq!(
            result,
            Err(RegistryError::RoomAlreadyExists {
                room_id: "demo-room".into(),
            })
        );
    }

    #[test]
    fn registry_allows_reuse_after_expiry() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let response = registry
            .put("demo-room", sample_request(), 1_601)
            .expect("expired room id should become reusable");

        assert_eq!(response.expires_at_epoch_seconds, 2_201);
    }

    #[test]
    fn registry_refreshes_live_room_when_ticket_matches() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut refresh = sample_request();
        refresh.ttl_seconds = 1200;
        let response = registry
            .put("demo-room", refresh, 1_100)
            .expect("matching ticket should refresh existing room");

        assert_eq!(response.expires_at_epoch_seconds, 2_300);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_purges_expired_entries() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-1", sample_request(), 1_000)
            .expect("first put should succeed");
        registry
            .put("demo-2", sample_request(), 1_100)
            .expect("second put should succeed");

        let purged = registry.purge_expired(1_700);
        assert_eq!(purged, 2);
        assert!(registry.is_empty());
    }

    #[test]
    fn ttl_is_capped_to_maximum() {
        let mut registry = RoomRegistry::new();
        let mut request = sample_request();
        request.ttl_seconds = MAX_TTL_SECONDS + 500;

        let response = registry
            .put("demo-room", request, 5_000)
            .expect("put should succeed");

        assert_eq!(response.expires_at_epoch_seconds, 5_000 + MAX_TTL_SECONDS);
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let mut registry = RoomRegistry::new();
        let mut request = sample_request();
        request.ttl_seconds = 0;

        let result = registry.put("demo-room", request, 5_000);
        assert_eq!(result, Err(RegistryError::InvalidTtl { ttl_seconds: 0 }));
    }
}
