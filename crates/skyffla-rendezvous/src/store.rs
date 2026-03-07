use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::{
    GetStreamResponse, PutStreamRequest, PutStreamResponse, RegistryError, StreamRecord,
    StreamRegistry,
};

pub trait StreamStore: Send + Sync {
    fn put(
        &self,
        stream_id: &str,
        request: PutStreamRequest,
        now_epoch_seconds: u64,
    ) -> Result<PutStreamResponse, StoreError>;

    fn get(&self, stream_id: &str, now_epoch_seconds: u64)
        -> Result<GetStreamResponse, StoreError>;

    fn delete(&self, stream_id: &str) -> Result<bool, StoreError>;

    fn purge_expired(&self, now_epoch_seconds: u64) -> Result<usize, StoreError>;
}

pub type SharedStreamStore = Arc<dyn StreamStore>;

#[derive(Debug)]
pub struct InMemoryStreamStore {
    inner: Mutex<StreamRegistry>,
}

impl InMemoryStreamStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(StreamRegistry::new()),
        }
    }
}

impl Default for InMemoryStreamStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamStore for InMemoryStreamStore {
    fn put(
        &self,
        stream_id: &str,
        request: PutStreamRequest,
        now_epoch_seconds: u64,
    ) -> Result<PutStreamResponse, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Storage("in-memory stream store lock poisoned".into()))?
            .put(stream_id, request, now_epoch_seconds)
            .map_err(StoreError::from)
    }

    fn get(
        &self,
        stream_id: &str,
        now_epoch_seconds: u64,
    ) -> Result<GetStreamResponse, StoreError> {
        self.inner
            .lock()
            .map_err(|_| StoreError::Storage("in-memory stream store lock poisoned".into()))?
            .get(stream_id, now_epoch_seconds)
            .map_err(StoreError::from)
    }

    fn delete(&self, stream_id: &str) -> Result<bool, StoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| StoreError::Storage("in-memory stream store lock poisoned".into()))?
            .delete(stream_id))
    }

    fn purge_expired(&self, now_epoch_seconds: u64) -> Result<usize, StoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| StoreError::Storage("in-memory stream store lock poisoned".into()))?
            .purge_expired(now_epoch_seconds))
    }
}

#[derive(Debug)]
pub struct SqliteStreamStore {
    connection: Mutex<Connection>,
}

impl SqliteStreamStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path).map_err(StoreError::from)?;
        Self::from_connection(connection)
    }

    pub fn new_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory().map_err(StoreError::from)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        self.with_connection(|connection| {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS streams (
                    stream_id TEXT PRIMARY KEY,
                    ticket TEXT NOT NULL,
                    capabilities_json TEXT NOT NULL,
                    expires_at_epoch_seconds INTEGER NOT NULL
                );",
            )?;
            Ok(())
        })
    }

    fn with_connection<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut guard = self
            .connection
            .lock()
            .map_err(|_| StoreError::Storage("sqlite stream store lock poisoned".into()))?;
        f(&mut guard)
    }
}

impl StreamStore for SqliteStreamStore {
    fn put(
        &self,
        stream_id: &str,
        request: PutStreamRequest,
        now_epoch_seconds: u64,
    ) -> Result<PutStreamResponse, StoreError> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM streams WHERE expires_at_epoch_seconds <= ?1",
                params![now_epoch_seconds as i64],
            )?;

            let ttl_seconds = request.normalized_ttl_seconds()?;
            let expires_at_epoch_seconds = now_epoch_seconds + ttl_seconds;
            let existing_ticket: Option<String> = connection
                .query_row(
                    "SELECT ticket FROM streams WHERE stream_id = ?1",
                    params![stream_id],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(ticket) = existing_ticket {
                if ticket != request.ticket {
                    return Err(StoreError::Registry(RegistryError::StreamAlreadyExists {
                        stream_id: stream_id.to_string(),
                    }));
                }

                let capabilities_json =
                    serde_json::to_string(&request.capabilities).map_err(StoreError::from)?;
                connection.execute(
                    "UPDATE streams
                     SET capabilities_json = ?1, expires_at_epoch_seconds = ?2
                     WHERE stream_id = ?3",
                    params![
                        capabilities_json,
                        expires_at_epoch_seconds as i64,
                        stream_id
                    ],
                )?;
            } else {
                let capabilities_json =
                    serde_json::to_string(&request.capabilities).map_err(StoreError::from)?;
                connection.execute(
                    "INSERT INTO streams (stream_id, ticket, capabilities_json, expires_at_epoch_seconds)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        stream_id,
                        request.ticket,
                        capabilities_json,
                        expires_at_epoch_seconds as i64
                    ],
                )?;
            }

            Ok(PutStreamResponse {
                stream_id: stream_id.to_string(),
                expires_at_epoch_seconds,
                status: crate::StreamStatus::Waiting,
            })
        })
    }

    fn get(
        &self,
        stream_id: &str,
        now_epoch_seconds: u64,
    ) -> Result<GetStreamResponse, StoreError> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM streams WHERE expires_at_epoch_seconds <= ?1",
                params![now_epoch_seconds as i64],
            )?;

            let record: Option<StreamRecord> = connection
                .query_row(
                    "SELECT stream_id, ticket, capabilities_json, expires_at_epoch_seconds
                     FROM streams
                     WHERE stream_id = ?1",
                    params![stream_id],
                    |row| {
                        let capabilities_json: String = row.get(2)?;
                        let capabilities =
                            serde_json::from_str(&capabilities_json).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    2,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        Ok(StreamRecord {
                            stream_id: row.get(0)?,
                            ticket: row.get(1)?,
                            capabilities,
                            expires_at_epoch_seconds: row.get::<_, i64>(3)? as u64,
                        })
                    },
                )
                .optional()?;

            record.map(|entry| entry.as_get_response()).ok_or_else(|| {
                StoreError::Registry(RegistryError::StreamNotFound {
                    stream_id: stream_id.to_string(),
                })
            })
        })
    }

    fn delete(&self, stream_id: &str) -> Result<bool, StoreError> {
        self.with_connection(|connection| {
            let deleted = connection.execute(
                "DELETE FROM streams WHERE stream_id = ?1",
                params![stream_id],
            )?;
            Ok(deleted > 0)
        })
    }

    fn purge_expired(&self, now_epoch_seconds: u64) -> Result<usize, StoreError> {
        self.with_connection(|connection| {
            let deleted = connection.execute(
                "DELETE FROM streams WHERE expires_at_epoch_seconds <= ?1",
                params![now_epoch_seconds as i64],
            )?;
            Ok(deleted)
        })
    }
}

#[derive(Debug)]
pub enum StoreError {
    Registry(RegistryError),
    Storage(String),
}

impl StoreError {
    pub fn as_registry_error(&self) -> Option<&RegistryError> {
        match self {
            Self::Registry(error) => Some(error),
            Self::Storage(_) => None,
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(error) => write!(f, "{error}"),
            Self::Storage(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<RegistryError> for StoreError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Storage(format!("sqlite error: {value}"))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Storage(format!("serde json error: {value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_TTL_SECONDS, MAX_TTL_SECONDS};
    use skyffla_protocol::Capabilities;

    fn sample_request() -> PutStreamRequest {
        PutStreamRequest {
            ticket: "bootstrap-ticket".into(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn sqlite_store_put_get_and_delete_round_trip() {
        let store = SqliteStreamStore::new_in_memory().expect("store should initialize");
        store
            .put("demo-room", sample_request(), 1_000)
            .expect("put should succeed");

        let resolved = store
            .get("demo-room", 1_001)
            .expect("get should resolve stored stream");
        assert_eq!(resolved.stream_id, "demo-room");

        assert!(store.delete("demo-room").expect("delete should succeed"));
        let missing = store.get("demo-room", 1_001);
        assert!(matches!(
            missing,
            Err(StoreError::Registry(RegistryError::StreamNotFound { .. }))
        ));
    }

    #[test]
    fn sqlite_store_rejects_conflicting_live_host() {
        let store = SqliteStreamStore::new_in_memory().expect("store should initialize");
        store
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut conflicting = sample_request();
        conflicting.ticket = "different-ticket".into();
        let result = store.put("demo-room", conflicting, 1_050);
        assert!(matches!(
            result,
            Err(StoreError::Registry(
                RegistryError::StreamAlreadyExists { .. }
            ))
        ));
    }

    #[test]
    fn sqlite_store_refreshes_matching_ticket() {
        let store = SqliteStreamStore::new_in_memory().expect("store should initialize");
        store
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let mut refresh = sample_request();
        refresh.ttl_seconds = MAX_TTL_SECONDS + 10;
        let response = store
            .put("demo-room", refresh, 1_100)
            .expect("matching ticket should refresh");
        assert_eq!(response.expires_at_epoch_seconds, 1_100 + MAX_TTL_SECONDS);
    }

    #[test]
    fn sqlite_store_purges_expired_rows() {
        let store = SqliteStreamStore::new_in_memory().expect("store should initialize");
        store
            .put("demo-room", sample_request(), 1_000)
            .expect("first put should succeed");

        let purged = store.purge_expired(1_700).expect("purge should succeed");
        assert_eq!(purged, 1);
    }
}
