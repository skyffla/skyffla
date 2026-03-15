use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::{request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Serialize;

use crate::store::{SharedRoomStore, StoreError};
use crate::{unix_timestamp_seconds, PutRoomRequest};

#[derive(Clone)]
pub struct AppState {
    pub store: SharedRoomStore,
    pub rate_limiter: Arc<IpRateLimiter>,
    pub trust_proxy_headers: bool,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/v1/rooms/{room_id}",
            put(put_room).get(get_room).delete(delete_room),
        )
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn put_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    MaybeConnectInfo(peer_addr): MaybeConnectInfo,
    Json(request): Json<PutRoomRequest>,
) -> Result<Json<crate::PutRoomResponse>, ApiError> {
    let now_epoch_seconds = unix_timestamp_seconds().map_err(ApiError::internal_time)?;
    let client_ip = client_ip_from_request(&headers, peer_addr, state.trust_proxy_headers);
    state.rate_limiter.check(client_ip, now_epoch_seconds)?;

    let response = state
        .store
        .put(&room_id, request, now_epoch_seconds)
        .map_err(ApiError::from_store)?;
    log_request("register", &room_id, client_ip, StatusCode::OK, "ok");
    Ok(Json(response))
}

async fn get_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    MaybeConnectInfo(peer_addr): MaybeConnectInfo,
) -> Result<Json<crate::GetRoomResponse>, ApiError> {
    let now_epoch_seconds = unix_timestamp_seconds().map_err(ApiError::internal_time)?;
    let client_ip = client_ip_from_request(&headers, peer_addr, state.trust_proxy_headers);
    state.rate_limiter.check(client_ip, now_epoch_seconds)?;

    let response = state
        .store
        .get(&room_id, now_epoch_seconds)
        .map_err(|error| {
            let api_error = ApiError::from_store(error);
            log_request(
                "resolve",
                &room_id,
                client_ip,
                api_error.status,
                api_error.code,
            );
            api_error
        })?;
    log_request("resolve", &room_id, client_ip, StatusCode::OK, "ok");
    Ok(Json(response))
}

async fn delete_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    MaybeConnectInfo(peer_addr): MaybeConnectInfo,
) -> Result<StatusCode, ApiError> {
    let now_epoch_seconds = unix_timestamp_seconds().map_err(ApiError::internal_time)?;
    let client_ip = client_ip_from_request(&headers, peer_addr, state.trust_proxy_headers);
    state.rate_limiter.check(client_ip, now_epoch_seconds)?;

    state.store.delete(&room_id).map_err(|error| {
        let api_error = ApiError::from_store(error);
        log_request(
            "delete",
            &room_id,
            client_ip,
            api_error.status,
            api_error.code,
        );
        api_error
    })?;
    log_request("delete", &room_id, client_ip, StatusCode::NO_CONTENT, "ok");
    Ok(StatusCode::NO_CONTENT)
}

fn client_ip_from_request(
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
    trust_proxy_headers: bool,
) -> IpAddr {
    if trust_proxy_headers {
        if let Some(forwarded_ip) = forwarded_ip_from_headers(headers) {
            return forwarded_ip;
        }
    }

    peer_addr
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn forwarded_ip_from_headers(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .and_then(|value| value.parse().ok())
}

struct MaybeConnectInfo(Option<SocketAddr>);

impl<S> FromRequestParts<S> for MaybeConnectInfo
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

fn log_request(
    operation: &str,
    room_id: &str,
    client_ip: IpAddr,
    status: StatusCode,
    outcome: &str,
) {
    println!(
        "request op={} room_id={} client_ip={} status={} outcome={}",
        operation,
        room_id,
        client_ip,
        status.as_u16(),
        outcome
    );
}

#[derive(Debug)]
pub struct IpRateLimiter {
    limit: usize,
    window_seconds: u64,
    buckets: Mutex<HashMap<IpAddr, VecDeque<u64>>>,
}

impl IpRateLimiter {
    pub fn new(limit: usize, window_seconds: u64) -> Self {
        Self {
            limit,
            window_seconds,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, ip: IpAddr, now_epoch_seconds: u64) -> Result<(), ApiError> {
        let window_start = now_epoch_seconds.saturating_sub(self.window_seconds);
        let mut buckets = self
            .buckets
            .lock()
            .map_err(|_| ApiError::internal("rate limiter lock poisoned"))?;
        let bucket = buckets.entry(ip).or_default();

        while bucket.front().copied().is_some_and(|ts| ts <= window_start) {
            bucket.pop_front();
        }

        if bucket.len() >= self.limit {
            let retry_after = bucket
                .front()
                .copied()
                .map(|ts| {
                    (ts + self.window_seconds)
                        .saturating_sub(now_epoch_seconds)
                        .max(1)
                })
                .unwrap_or(1);
            return Err(ApiError::rate_limited(retry_after));
        }

        bucket.push_back(now_epoch_seconds);
        Ok(())
    }
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after_seconds: Option<u64>,
}

impl ApiError {
    fn from_store(error: StoreError) -> Self {
        match error.as_registry_error() {
            Some(crate::RegistryError::InvalidTtl { ttl_seconds }) => Self {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_ttl",
                message: format!("ttl_seconds must be greater than zero, got {ttl_seconds}"),
                retry_after_seconds: None,
            },
            Some(crate::RegistryError::RoomAlreadyExists { room_id }) => Self {
                status: StatusCode::CONFLICT,
                code: "room_already_exists",
                message: format!("room {room_id} is already hosted"),
                retry_after_seconds: None,
            },
            Some(crate::RegistryError::RoomNotFound { room_id }) => Self {
                status: StatusCode::NOT_FOUND,
                code: "room_not_found",
                message: format!("room {room_id} was not found"),
                retry_after_seconds: None,
            },
            None => Self::internal(error.to_string()),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    fn internal_time(error: std::time::SystemTimeError) -> Self {
        Self::internal(format!("system clock error: {error}"))
    }

    fn rate_limited(retry_after_seconds: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: format!(
                "too many requests from this client, retry after {retry_after_seconds} seconds"
            ),
            retry_after_seconds: Some(retry_after_seconds),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorResponse {
                error: self.code,
                message: self.message,
            }),
        )
            .into_response();

        if let Some(retry_after_seconds) = self.retry_after_seconds {
            let value = retry_after_seconds.to_string();
            if let Ok(header_value) = value.parse() {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, header_value);
            }
        }

        response
    }
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use skyffla_protocol::Capabilities;
    use tower::ServiceExt;

    use super::*;
    use crate::store::InMemoryRoomStore;

    fn test_app(rate_limit: usize, trust_proxy_headers: bool) -> Router {
        build_router(AppState {
            store: Arc::new(InMemoryRoomStore::new()),
            rate_limiter: Arc::new(IpRateLimiter::new(rate_limit, 60)),
            trust_proxy_headers,
        })
    }

    #[tokio::test]
    async fn put_then_get_room_via_http() {
        let app = test_app(10, false);

        let put_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&crate::PutRoomRequest {
                            ticket: "bootstrap-ticket".into(),
                            ttl_seconds: 600,
                            capabilities: Capabilities::default(),
                        })
                        .expect("request should serialize"),
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        assert_eq!(put_response.status(), StatusCode::OK);

        let get_response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/demo-room")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        assert_eq!(get_response.status(), StatusCode::OK);
        let body = get_response
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();
        let response: crate::GetRoomResponse =
            serde_json::from_slice(&body).expect("response should deserialize");
        assert_eq!(response.room_id, "demo-room");
    }

    #[tokio::test]
    async fn conflicting_room_returns_conflict() {
        let app = test_app(10, false);

        let first_body = serde_json::to_vec(&crate::PutRoomRequest {
            ticket: "ticket-a".into(),
            ttl_seconds: 600,
            capabilities: Capabilities::default(),
        })
        .expect("request should serialize");
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .body(Body::from(first_body))
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        let second_body = serde_json::to_vec(&crate::PutRoomRequest {
            ticket: "ticket-b".into(),
            ttl_seconds: 600,
            capabilities: Capabilities::default(),
        })
        .expect("request should serialize");
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .body(Body::from(second_body))
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn rate_limit_returns_too_many_requests() {
        let app = test_app(1, false);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(first.status(), StatusCode::NOT_FOUND);

        let second = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn ignores_forwarded_for_headers_by_default() {
        let app = test_app(1, false);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .header("x-forwarded-for", "203.0.113.10")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(first.status(), StatusCode::NOT_FOUND);

        let second = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .header("x-forwarded-for", "203.0.113.11")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn trusts_forwarded_for_headers_when_enabled() {
        let app = test_app(1, true);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .header("x-forwarded-for", "203.0.113.10")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(first.status(), StatusCode::NOT_FOUND);

        let second = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/rooms/missing")
                    .header("x-forwarded-for", "203.0.113.11")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(second.status(), StatusCode::NOT_FOUND);
    }
}
