pub mod app;
pub mod store;

use std::collections::BTreeMap;
use std::fmt;

use iroh_base::EndpointAddr;
use serde::{Deserialize, Serialize};
use skyffla_protocol::{Capabilities, ProtocolVersion};

pub const DEFAULT_TTL_SECONDS: u64 = 600;
pub const MAX_TTL_SECONDS: u64 = 3600;
pub const RENDEZVOUS_API_VERSION: ProtocolVersion = ProtocolVersion::new(3, 0);
pub const RENDEZVOUS_VERSION_HEADER: &str = "x-skyffla-rendezvous-version";
pub const MAX_REQUEST_BYTES: usize = 4096;
pub const MAX_TICKET_BYTES: usize = 2048;
pub const MAX_ROOMS: usize = 1000;

pub type OwnerHash = [u8; 32];

pub fn validate_room_id(room_id: &str) -> Result<(), RegistryError> {
    if room_id.is_empty()
        || room_id.len() > 64
        || !room_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(RegistryError::InvalidRequest(
            "room id must be 1-64 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutRoomRequest {
    pub ticket: String,
    pub ttl_seconds: u64,
    pub capabilities: Capabilities,
}

impl PutRoomRequest {
    pub fn validate(&mut self, room_id: &str) -> Result<(), RegistryError> {
        validate_room_id(room_id)?;
        if self.ticket.len() > MAX_TICKET_BYTES || self.capabilities.transport.len() > 2 {
            return Err(RegistryError::InvalidRequest(
                "registration exceeds field limits",
            ));
        }
        let addr: EndpointAddr = serde_json::from_str(&self.ticket).map_err(|_| {
            RegistryError::InvalidRequest("ticket must be an iroh endpoint address")
        })?;
        if addr.addrs.len() > 16
            || addr.relay_urls().any(|url| {
                url.as_str().len() > 256
                    || url.scheme() != "https"
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
            })
        {
            return Err(RegistryError::InvalidRequest(
                "ticket contains unsupported addresses",
            ));
        }
        // Discard unknown fields and formatting instead of storing opaque caller data.
        self.ticket = serde_json::to_string(&addr)
            .map_err(|_| RegistryError::InvalidRequest("invalid ticket"))?;
        Ok(())
    }

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
    pub owner_hash: OwnerHash,
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
        mut request: PutRoomRequest,
        owner_hash: OwnerHash,
        now_epoch_seconds: u64,
    ) -> Result<PutRoomResponse, RegistryError> {
        let room_id = room_id.into();
        request.validate(&room_id)?;
        self.purge_expired(now_epoch_seconds);

        let ttl_seconds = request.normalized_ttl_seconds()?;
        let expires_at_epoch_seconds = now_epoch_seconds + ttl_seconds;
        if let Some(existing) = self.entries.get_mut(&room_id) {
            if existing.owner_hash != owner_hash {
                return Err(RegistryError::RoomAlreadyExists {
                    room_id: room_id.clone(),
                });
            }

            existing.ticket = request.ticket;
            existing.capabilities = request.capabilities;
            existing.expires_at_epoch_seconds = expires_at_epoch_seconds;
        } else {
            if self.entries.len() >= MAX_ROOMS {
                return Err(RegistryError::CapacityExceeded);
            }
            let record = RoomRecord {
                room_id: room_id.clone(),
                ticket: request.ticket,
                capabilities: request.capabilities,
                expires_at_epoch_seconds,
                owner_hash,
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
        validate_room_id(room_id)?;
        self.purge_expired(now_epoch_seconds);

        self.entries
            .get(room_id)
            .map(RoomRecord::as_get_response)
            .ok_or_else(|| RegistryError::RoomNotFound {
                room_id: room_id.to_string(),
            })
    }

    pub fn delete(&mut self, room_id: &str, owner_hash: OwnerHash) -> Result<bool, RegistryError> {
        validate_room_id(room_id)?;
        if self
            .entries
            .get(room_id)
            .is_some_and(|room| room.owner_hash != owner_hash)
        {
            return Err(RegistryError::NotOwner);
        }
        Ok(self.entries.remove(room_id).is_some())
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
    InvalidRequest(&'static str),
    NotOwner,
    CapacityExceeded,
    InvalidTtl { ttl_seconds: u64 },
    RoomAlreadyExists { room_id: String },
    RoomNotFound { room_id: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "{message}"),
            Self::NotOwner => write!(f, "room ownership token does not match"),
            Self::CapacityExceeded => write!(f, "room registry is full; retry after rooms expire"),
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
pub(crate) fn test_ticket(seed: u8) -> String {
    serde_json::to_string(&EndpointAddr::new(
        iroh_base::SecretKey::from_bytes(&[seed; 32]).public(),
    ))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> PutRoomRequest {
        PutRoomRequest {
            ticket: test_ticket(1),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn registration_validation_bounds_ids_tickets_and_capabilities() {
        for room_id in ["", "with space", "bad\nlog", &"x".repeat(65)] {
            assert!(sample_request().validate(room_id).is_err());
        }
        let mut request = sample_request();
        request.ticket = "x".repeat(MAX_TICKET_BYTES + 1);
        assert!(request.validate("valid").is_err());
        let mut request = sample_request();
        request.capabilities.transport =
            vec![skyffla_protocol::TransportCapability::NativeDirect; 3];
        assert!(request.validate("valid").is_err());
    }

    #[test]
    fn ticket_validation_discards_unknown_data_and_rejects_oversized_address_lists() {
        let mut request = sample_request();
        let mut value: serde_json::Value = serde_json::from_str(&request.ticket).unwrap();
        value["unrelated"] = "arbitrary stored payload".into();
        request.ticket = value.to_string();
        request.validate("valid").unwrap();
        assert_eq!(request.ticket, test_ticket(1));
        let mut addr: EndpointAddr = serde_json::from_str(&request.ticket).unwrap();
        for port in 1..=17 {
            addr = addr.with_ip_addr(([127, 0, 0, 1], port).into());
        }
        request.ticket = serde_json::to_string(&addr).unwrap();
        assert!(request.validate("valid").is_err());
    }

    #[test]
    fn put_and_get_room_round_trip() {
        let mut registry = RoomRegistry::new();
        let response = registry
            .put("demo-room", sample_request(), [1; 32], 1_000)
            .expect("put should succeed");

        assert_eq!(response.room_id, "demo-room");
        assert_eq!(response.status, RoomStatus::Waiting);

        let resolved = registry
            .get("demo-room", 1_001)
            .expect("get should resolve existing room");
        assert_eq!(resolved.ticket, test_ticket(1));
        assert_eq!(resolved.capabilities, Capabilities::default());
    }

    #[test]
    fn registry_rejects_duplicate_live_room() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-room", sample_request(), [1; 32], 1_000)
            .expect("first put should succeed");

        let mut conflicting = sample_request();
        conflicting.ticket = test_ticket(2);
        let result = registry.put("demo-room", conflicting, [2; 32], 1_100);
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
            .put("demo-room", sample_request(), [1; 32], 1_000)
            .expect("first put should succeed");

        let response = registry
            .put("demo-room", sample_request(), [1; 32], 1_601)
            .expect("expired room id should become reusable");

        assert_eq!(response.expires_at_epoch_seconds, 2_201);
    }

    #[test]
    fn registry_refreshes_live_room_when_owner_matches() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-room", sample_request(), [1; 32], 1_000)
            .expect("first put should succeed");

        let mut refresh = sample_request();
        refresh.ttl_seconds = 1200;
        let response = registry
            .put("demo-room", refresh, [1; 32], 1_100)
            .expect("matching ticket should refresh existing room");

        assert_eq!(response.expires_at_epoch_seconds, 2_300);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_purges_expired_entries() {
        let mut registry = RoomRegistry::new();
        registry
            .put("demo-1", sample_request(), [1; 32], 1_000)
            .expect("first put should succeed");
        registry
            .put("demo-2", sample_request(), [1; 32], 1_100)
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
            .put("demo-room", request, [1; 32], 5_000)
            .expect("put should succeed");

        assert_eq!(response.expires_at_epoch_seconds, 5_000 + MAX_TTL_SECONDS);
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let mut registry = RoomRegistry::new();
        let mut request = sample_request();
        request.ttl_seconds = 0;

        let result = registry.put("demo-room", request, [1; 32], 5_000);
        assert_eq!(result, Err(RegistryError::InvalidTtl { ttl_seconds: 0 }));
    }
}
