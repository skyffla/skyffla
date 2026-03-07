pub mod app;
pub mod store;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use skyffla_protocol::Capabilities;

pub const DEFAULT_TTL_SECONDS: u64 = 600;
pub const MAX_TTL_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutStreamRequest {
    pub ticket: String,
    pub ttl_seconds: u64,
    pub capabilities: Capabilities,
}

impl PutStreamRequest {
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
pub struct PutStreamResponse {
    pub stream_id: String,
    pub expires_at_epoch_seconds: u64,
    pub status: StreamStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetStreamResponse {
    pub stream_id: String,
    pub ticket: String,
    pub expires_at_epoch_seconds: u64,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamStatus {
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRecord {
    pub stream_id: String,
    pub ticket: String,
    pub capabilities: Capabilities,
    pub expires_at_epoch_seconds: u64,
}

impl StreamRecord {
    pub fn is_expired_at(&self, now_epoch_seconds: u64) -> bool {
        self.expires_at_epoch_seconds <= now_epoch_seconds
    }

    pub fn as_get_response(&self) -> GetStreamResponse {
        GetStreamResponse {
            stream_id: self.stream_id.clone(),
            ticket: self.ticket.clone(),
            expires_at_epoch_seconds: self.expires_at_epoch_seconds,
            capabilities: self.capabilities.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct StreamRegistry {
    entries: BTreeMap<String, StreamRecord>,
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(
        &mut self,
        stream_id: impl Into<String>,
        request: PutStreamRequest,
        now_epoch_seconds: u64,
    ) -> Result<PutStreamResponse, RegistryError> {
        let stream_id = stream_id.into();
        self.purge_expired(now_epoch_seconds);

        let ttl_seconds = request.normalized_ttl_seconds()?;
        let expires_at_epoch_seconds = now_epoch_seconds + ttl_seconds;
        if let Some(existing) = self.entries.get_mut(&stream_id) {
            if existing.ticket != request.ticket {
                return Err(RegistryError::StreamAlreadyExists {
                    stream_id: stream_id.clone(),
                });
            }

            existing.capabilities = request.capabilities;
            existing.expires_at_epoch_seconds = expires_at_epoch_seconds;
        } else {
            let record = StreamRecord {
                stream_id: stream_id.clone(),
                ticket: request.ticket,
                capabilities: request.capabilities,
                expires_at_epoch_seconds,
            };
            self.entries.insert(stream_id.clone(), record);
        }

        Ok(PutStreamResponse {
            stream_id,
            expires_at_epoch_seconds,
            status: StreamStatus::Waiting,
        })
    }

    pub fn get(
        &mut self,
        stream_id: &str,
        now_epoch_seconds: u64,
    ) -> Result<GetStreamResponse, RegistryError> {
        self.purge_expired(now_epoch_seconds);

        self.entries
            .get(stream_id)
            .map(StreamRecord::as_get_response)
            .ok_or_else(|| RegistryError::StreamNotFound {
                stream_id: stream_id.to_string(),
            })
    }

    pub fn delete(&mut self, stream_id: &str) -> bool {
        self.entries.remove(stream_id).is_some()
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
    StreamAlreadyExists { stream_id: String },
    StreamNotFound { stream_id: String },
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
            Self::StreamAlreadyExists { stream_id } => {
                write!(f, "stream {stream_id} is already hosted")
            }
            Self::StreamNotFound { stream_id } => write!(f, "stream {stream_id} was not found"),
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

    fn sample_request() -> PutStreamRequest {
        PutStreamRequest {
            ticket: "bootstrap-ticket".into(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn put_and_get_stream_round_trip() {
        let mut registry = StreamRegistry::new();
        let response = registry
            .put("demo-room", sample_request(), 1_000)
            .expect("put should succeed");

        assert_eq!(response.stream_id, "demo-room");
        assert_eq!(response.status, StreamStatus::Waiting);

        let resolved = registry
            .get("demo-room", 1_001)
            .expect("get should resolve existing stream");
        assert_eq!(resolved.ticket, "bootstrap-ticket");
        assert_eq!(resolved.capabilities, Capabilities::default());
    }

    #[test]
    fn registry_rejects_duplicate_live_stream() {
        let mut registry = StreamRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut conflicting = sample_request();
        conflicting.ticket = "different-ticket".into();
        let result = registry.put("demo-room", conflicting, 1_100);
        assert_eq!(
            result,
            Err(RegistryError::StreamAlreadyExists {
                stream_id: "demo-room".into(),
            })
        );
    }

    #[test]
    fn registry_allows_reuse_after_expiry() {
        let mut registry = StreamRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let response = registry
            .put("demo-room", sample_request(), 1_601)
            .expect("expired stream id should become reusable");

        assert_eq!(response.expires_at_epoch_seconds, 2_201);
    }

    #[test]
    fn registry_refreshes_live_stream_when_ticket_matches() {
        let mut registry = StreamRegistry::new();
        registry
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut refresh = sample_request();
        refresh.ttl_seconds = 1200;
        let response = registry
            .put("demo-room", refresh, 1_100)
            .expect("matching ticket should refresh existing stream");

        assert_eq!(response.expires_at_epoch_seconds, 2_300);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_purges_expired_entries() {
        let mut registry = StreamRegistry::new();
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
        let mut registry = StreamRegistry::new();
        let mut request = sample_request();
        request.ttl_seconds = MAX_TTL_SECONDS + 500;

        let response = registry
            .put("demo-room", request, 5_000)
            .expect("put should succeed");

        assert_eq!(response.expires_at_epoch_seconds, 5_000 + MAX_TTL_SECONDS);
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let mut registry = StreamRegistry::new();
        let mut request = sample_request();
        request.ttl_seconds = 0;

        let result = registry.put("demo-room", request, 5_000);
        assert_eq!(result, Err(RegistryError::InvalidTtl { ttl_seconds: 0 }));
    }
}
