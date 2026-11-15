use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Request, State};
use axum::http::{header::HeaderValue, request::Parts, HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::store::{SharedRoomStore, StoreError};
use crate::{
    unix_timestamp_seconds, OwnerHash, PutRoomRequest, MAX_REQUEST_BYTES, RENDEZVOUS_API_VERSION,
    RENDEZVOUS_VERSION_HEADER,
};

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
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(from_fn_with_state(state.clone(), limit_requests))
        .layer(map_response(add_version_header))
        .with_state(state)
}

async fn limit_requests(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let now = match unix_timestamp_seconds() {
        Ok(now) => now,
        Err(error) => return ApiError::internal_time(error).into_response(),
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let ip = client_ip_from_request(request.headers(), peer, state.trust_proxy_headers);
    if let Err(error) = state.rate_limiter.check(ip, now) {
        return error.into_response();
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

fn owner_hash(headers: &HeaderMap) -> Result<OwnerHash, ApiError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|_| {
            headers
                .get_all(axum::http::header::AUTHORIZATION)
                .iter()
                .count()
                == 1
        })
        .filter(|value| value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "ownership_token_required",
            message: "a 256-bit room ownership token is required".into(),
            retry_after_seconds: None,
        })?;
    Ok(Sha256::digest(token.as_bytes()).into())
}

async fn health() -> &'static str {
    "ok"
}

async fn add_version_header(mut response: Response) -> Response {
    let value = HeaderValue::from_str(&RENDEZVOUS_API_VERSION.to_string())
        .expect("static rendezvous version should serialize as a header");
    response
        .headers_mut()
        .insert(RENDEZVOUS_VERSION_HEADER, value);
    response
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
    let owner_hash = owner_hash(&headers)?;

    let response = state
        .store
        .put(&room_id, request, owner_hash, now_epoch_seconds)
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
    crate::validate_room_id(&room_id).map_err(|error| ApiError::from_store(error.into()))?;
    let now_epoch_seconds = unix_timestamp_seconds().map_err(ApiError::internal_time)?;
    let client_ip = client_ip_from_request(&headers, peer_addr, state.trust_proxy_headers);

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
    crate::validate_room_id(&room_id).map_err(|error| ApiError::from_store(error.into()))?;
    let client_ip = client_ip_from_request(&headers, peer_addr, state.trust_proxy_headers);
    let owner_hash = owner_hash(&headers)?;

    state.store.delete(&room_id, owner_hash).map_err(|error| {
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
    if trust_proxy_headers && peer_addr.is_some_and(|addr| addr.ip().is_loopback()) {
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
    state: Mutex<RateLimitState>,
}

#[derive(Debug, Default)]
struct RateLimitState {
    buckets: HashMap<IpAddr, VecDeque<u64>>,
    global: VecDeque<u64>,
}

const MAX_CLIENT_BUCKETS: usize = 4096;
const GLOBAL_REQUEST_LIMIT: usize = 600;

impl IpRateLimiter {
    pub fn new(limit: usize, window_seconds: u64) -> Self {
        Self {
            limit,
            window_seconds,
            state: Mutex::new(RateLimitState::default()),
        }
    }

    pub fn check(&self, ip: IpAddr, now_epoch_seconds: u64) -> Result<(), ApiError> {
        let window_start = now_epoch_seconds.saturating_sub(self.window_seconds);
        let mut state = self
            .state
            .lock()
            .map_err(|_| ApiError::internal("rate limiter lock poisoned"))?;
        state
            .buckets
            .retain(|_, bucket| bucket.back().is_some_and(|ts| *ts > window_start));
        while state.global.front().is_some_and(|ts| *ts <= window_start) {
            state.global.pop_front();
        }
        if state.global.len() >= GLOBAL_REQUEST_LIMIT
            || (!state.buckets.contains_key(&ip) && state.buckets.len() >= MAX_CLIENT_BUCKETS)
        {
            return Err(ApiError::rate_limited(self.window_seconds.max(1)));
        }
        let bucket = state.buckets.entry(ip).or_default();

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
        state.global.push_back(now_epoch_seconds);
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
            Some(crate::RegistryError::InvalidRequest(message)) => Self {
                status: StatusCode::BAD_REQUEST,
                code: "invalid_registration",
                message: (*message).into(),
                retry_after_seconds: None,
            },
            Some(crate::RegistryError::NotOwner) => Self {
                status: StatusCode::FORBIDDEN,
                code: "not_room_owner",
                message: "room ownership token does not match".into(),
                retry_after_seconds: None,
            },
            Some(crate::RegistryError::CapacityExceeded) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "registry_full",
                message: "room registry is full; retry after rooms expire".into(),
                retry_after_seconds: Some(60),
            },
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
            None => {
                eprintln!("room storage error: {error}");
                Self::internal("room storage unavailable")
            }
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

    fn room_request(method: &str, token: Option<char>, ticket: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri("/v1/rooms/security-test")
            .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header(
                "authorization",
                format!("Bearer {}", token.to_string().repeat(64)),
            );
        }
        request
            .body(Body::from(
                serde_json::to_vec(&PutRoomRequest {
                    ticket: ticket.into(),
                    ttl_seconds: 600,
                    capabilities: Capabilities::default(),
                })
                .unwrap(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn public_lookup_does_not_allow_a_stranger_to_refresh_delete_or_replace_the_host() {
        for store in [
            Arc::new(InMemoryRoomStore::new()) as SharedRoomStore,
            Arc::new(crate::store::SqliteRoomStore::new_in_memory().unwrap()),
        ] {
            let app = build_router(AppState {
                store,
                rate_limiter: Arc::new(IpRateLimiter::new(120, 60)),
                trust_proxy_headers: false,
            });
            let ticket = crate::test_ticket(1);
            assert_eq!(
                app.clone()
                    .oneshot(room_request("PUT", None, &ticket))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("PUT", Some('a'), &ticket))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            let public = app
                .clone()
                .oneshot(room_request("GET", None, ""))
                .await
                .unwrap();
            let bytes = public.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 4);
            assert_eq!(value["ticket"], ticket);
            assert_eq!(
                app.clone()
                    .oneshot(room_request("PUT", Some('b'), &ticket))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CONFLICT
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("DELETE", None, ""))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("DELETE", Some('b'), ""))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("PUT", Some('b'), &crate::test_ticket(2)))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CONFLICT
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("GET", None, ""))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert_eq!(
                app.clone()
                    .oneshot(room_request("DELETE", Some('a'), ""))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                app.oneshot(room_request("GET", None, ""))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn rejects_opaque_tickets_and_limits_bodies_before_json_parsing() {
        let app = test_app(10, false);
        assert_eq!(
            app.clone()
                .oneshot(room_request("PUT", Some('a'), "not-a-ticket"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.clone()
                .oneshot(room_request("PUT", Some('a'), &"x".repeat(5000)))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            app.oneshot(room_request("GET", None, ""))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn invalid_json_and_unknown_routes_still_consume_the_request_budget() {
        let app = test_app(1, false);
        let first = Request::builder()
            .method("PUT")
            .uri("/v1/rooms/missing")
            .header("content-type", "application/json")
            .body(Body::from("{"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(first).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let second = Request::builder()
            .uri("/unknown")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(second).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn direct_clients_cannot_spoof_a_trusted_local_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.1".parse().unwrap());
        let remote: SocketAddr = "192.0.2.1:1234".parse().unwrap();
        assert_eq!(
            client_ip_from_request(&headers, Some(remote), true),
            remote.ip()
        );
    }

    #[test]
    fn global_budget_bounds_distributed_requests_and_expired_ip_state_is_removed() {
        let limiter = IpRateLimiter::new(120, 60);
        for id in 1..=GLOBAL_REQUEST_LIMIT {
            limiter
                .check(IpAddr::V6((id as u128).into()), 1000)
                .unwrap();
        }
        assert!(limiter.check("192.0.2.1".parse().unwrap(), 1000).is_err());
        assert_eq!(
            limiter.state.lock().unwrap().buckets.len(),
            GLOBAL_REQUEST_LIMIT
        );
        limiter.check("192.0.2.1".parse().unwrap(), 1061).unwrap();
        assert_eq!(limiter.state.lock().unwrap().buckets.len(), 1);
    }

    #[tokio::test]
    async fn put_then_get_room_via_http() {
        let app = test_app(10, false);
        let version = RENDEZVOUS_API_VERSION.to_string();

        let put_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", "a".repeat(64)))
                    .body(Body::from(
                        serde_json::to_vec(&crate::PutRoomRequest {
                            ticket: crate::test_ticket(1),
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
        assert_eq!(
            put_response
                .headers()
                .get(RENDEZVOUS_VERSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(version.as_str())
        );

        let get_response = app
            .oneshot(
                Request::builder()
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                    .method("GET")
                    .uri("/v1/rooms/demo-room")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        assert_eq!(get_response.status(), StatusCode::OK);
        assert_eq!(
            get_response
                .headers()
                .get(RENDEZVOUS_VERSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(version.as_str())
        );
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
            ticket: crate::test_ticket(1),
            ttl_seconds: 600,
            capabilities: Capabilities::default(),
        })
        .expect("request should serialize");
        app.clone()
            .oneshot(
                Request::builder()
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", "a".repeat(64)))
                    .body(Body::from(first_body))
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");

        let second_body = serde_json::to_vec(&crate::PutRoomRequest {
            ticket: crate::test_ticket(2),
            ttl_seconds: 600,
            capabilities: Capabilities::default(),
        })
        .expect("request should serialize");
        let response = app
            .oneshot(
                Request::builder()
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
                    .method("PUT")
                    .uri("/v1/rooms/demo-room")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {}", "b".repeat(64)))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
                    .extension(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()))
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
