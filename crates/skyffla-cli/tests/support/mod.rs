#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use iroh::address_lookup::MdnsAddressLookup;
use skyffla_rendezvous::app::{build_router, AppState, IpRateLimiter};
use skyffla_rendezvous::store::InMemoryRoomStore;
use skyffla_transport::{IrohTransport, TransportError};
use tokio::process::Command;
use tokio::net::TcpListener;
use tokio::time::sleep;

pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(2);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const LOCAL_DISCOVERY_JOIN_ELECTION_WINDOW_MS: u64 = 300;
pub const LOCAL_DISCOVERY_HOST_ANNOUNCEMENT_GRACE_MS: u64 = 300;
pub const LOCAL_DISCOVERY_BOOTSTRAP_DELAY: Duration = Duration::from_millis(900);
pub const LOCAL_JOIN_PROMOTION_DELAY: Duration = Duration::from_millis(900);

pub fn apply_local_discovery_test_env(command: &mut Command) {
    command
        .env(
            "SKYFFLA_LOCAL_JOIN_ELECTION_WINDOW_MS",
            LOCAL_DISCOVERY_JOIN_ELECTION_WINDOW_MS.to_string(),
        )
        .env(
            "SKYFFLA_LOCAL_HOST_ANNOUNCEMENT_GRACE_MS",
            LOCAL_DISCOVERY_HOST_ANNOUNCEMENT_GRACE_MS.to_string(),
        );
}

pub async fn bind_transport_or_skip() -> Option<IrohTransport> {
    match IrohTransport::bind().await {
        Ok(transport) => Some(transport),
        Err(error) if is_transport_permission_error(&error) => None,
        Err(error) => panic!("iroh bind should succeed: {error}"),
    }
}

pub struct LocalDiscoveryTestGuard {
    _process_guard: MutexGuard<'static, ()>,
    file: File,
}

pub struct TestServer {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

pub fn acquire_local_discovery_test_guard() -> Result<LocalDiscoveryTestGuard> {
    let process_guard = local_discovery_process_guard()
        .lock()
        .expect("local discovery test mutex should not be poisoned");
    let lock_path = std::env::temp_dir().join("skyffla-local-discovery-tests.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open local discovery test lock {lock_path:?}"))?;
    file.lock()
        .with_context(|| format!("failed to lock local discovery test lock {lock_path:?}"))?;
    Ok(LocalDiscoveryTestGuard {
        _process_guard: process_guard,
        file,
    })
}

pub async fn local_discovery_available() -> Result<bool> {
    let Some(transport) = bind_transport_or_skip().await else {
        return Ok(false);
    };

    let mdns = match MdnsAddressLookup::builder()
        .advertise(false)
        .service_name(format!("skyffla-test-{}", unique_room_name()))
        .build(transport.endpoint().id())
    {
        Ok(mdns) => mdns,
        Err(error) if is_mdns_permission_error(&error.to_string()) => {
            transport.close().await;
            return Ok(false);
        }
        Err(error) => {
            transport.close().await;
            return Err(error).context("failed to initialize mdns discovery for test");
        }
    };

    transport.endpoint().address_lookup().add(mdns);
    transport.close().await;
    Ok(true)
}

impl Drop for LocalDiscoveryTestGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn local_discovery_process_guard() -> &'static Mutex<()> {
    static PROCESS_GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    PROCESS_GUARD.get_or_init(|| Mutex::new(()))
}

impl TestServer {
    pub async fn spawn() -> Result<Option<Self>> {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if is_socket_permission_error(&error) => return Ok(None),
            Err(error) => return Err(error).context("failed to bind rendezvous test listener"),
        };
        let Some(probe) = bind_transport_or_skip().await else {
            return Ok(None);
        };
        probe.close().await;
        let addr = listener
            .local_addr()
            .context("failed to read rendezvous test listener addr")?;
        let task = tokio::spawn(async move {
            let app = build_router(AppState {
                store: Arc::new(InMemoryRoomStore::new()),
                rate_limiter: Arc::new(IpRateLimiter::new(120, 60)),
                trust_proxy_headers: false,
            });
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("rendezvous server should stay healthy during test");
        });
        let url = format!("http://{addr}");
        wait_for_server_ready(&url).await?;
        Ok(Some(Self { url, task }))
    }

    pub fn abort(self) {
        self.task.abort();
    }
}

pub fn assert_local_mode_stderr(stderr: &[u8]) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(
        !stderr.contains("rendezvous_error"),
        "local mode unexpectedly referenced rendezvous:\n{stderr}"
    );
    assert!(
        !stderr.contains("127.0.0.1:9"),
        "local mode unexpectedly referenced server URL:\n{stderr}"
    );
}

pub fn assert_stdio_json_stderr(stderr: &[u8]) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(
        stderr.contains("\"event\":\"machine_open\""),
        "stderr did not contain machine_open event:\n{stderr}"
    );
    assert!(
        stderr.contains("\"event\":\"stdin_eof\""),
        "stderr did not contain stdin_eof event:\n{stderr}"
    );
    assert!(
        stderr.contains("\"event\":\"remote_eof\""),
        "stderr did not contain remote_eof event:\n{stderr}"
    );
    assert!(
        stderr.contains("\"event\":\"machine_closed\""),
        "stderr did not contain machine_closed event:\n{stderr}"
    );
}

pub fn fresh_test_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nonce}"))
}

pub fn unique_room_name() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    format!("local-room-{nonce}")
}

pub async fn wait_for_server_ready(server_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let health_url = format!("{server_url}/health");
    let deadline = tokio::time::Instant::now() + SERVER_READY_TIMEOUT;

    loop {
        if let Ok(response) = client.get(&health_url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "rendezvous server was not ready within {:?}",
                SERVER_READY_TIMEOUT
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub async fn wait_for_room_ready(server_url: &str, room: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let room_url = format!("{server_url}/v1/rooms/{room}");
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

    loop {
        if let Ok(response) = client.get(&room_url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "room {room} was not registered within {:?}",
                CONNECT_TIMEOUT
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn is_transport_permission_error(error: &TransportError) -> bool {
    matches!(error, TransportError::EndpointBind(bind_error) if {
        let message = bind_error.to_string();
        message.contains("Operation not permitted") || message.contains("Failed to bind sockets")
    })
}

fn is_socket_permission_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
    ) || error.to_string().contains("Operation not permitted")
}

fn is_mdns_permission_error(message: &str) -> bool {
    message.contains("Operation not permitted")
        || message.contains("permission denied")
        || message.contains("Addr not available")
}
