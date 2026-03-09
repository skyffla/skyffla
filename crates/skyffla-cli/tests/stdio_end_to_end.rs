use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use iroh::address_lookup::MdnsAddressLookup;
use skyffla_rendezvous::app::{build_router, AppState, IpRateLimiter};
use skyffla_rendezvous::store::InMemoryStreamStore;
use skyffla_transport::{IrohTransport, TransportError};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::sleep;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LOCAL_DISCOVERY_BOOTSTRAP_DELAY: Duration = Duration::from_millis(500);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_join_to_join_transfers_payload_end_to_end() -> Result<()> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) if is_socket_permission_error(&error) => return Ok(()),
        Err(error) => return Err(error).context("failed to bind rendezvous test listener"),
    };
    let Some(probe) = bind_transport_or_skip().await else {
        return Ok(());
    };
    probe.close().await;
    let addr = listener
        .local_addr()
        .context("failed to read rendezvous test listener addr")?;
    let server = tokio::spawn(async move {
        let app = build_router(AppState {
            store: Arc::new(InMemoryStreamStore::new()),
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

    let home_dir = fresh_test_dir("skyffla-cli-stdio");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let payload = b"hello from integration test\nsecond line\n";
    let room = unique_room_name();
    let server_url = format!("http://{addr}");
    let bin = env!("CARGO_BIN_EXE_skyffla");

    wait_for_server_ready(&server_url).await?;

    let mut host = Command::new(bin);
    host.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server_url)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into host stdin")?;
    drop(host_stdin);

    wait_for_stream_ready(&server_url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server_url)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("join process watchdog timed out")?
        .context("join process failed while waiting for output")?;
    if !join_output.status.success() {
        let _ = host.kill().await;
        let host_output = host
            .wait_with_output()
            .await
            .context("host process failed while collecting failure output")?;
        server.abort();
        anyhow::bail!(
            "join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = tokio::time::timeout(PROCESS_TIMEOUT, host.wait_with_output())
        .await
        .context("host process watchdog timed out")?
        .context("host process failed while waiting for output")?;

    server.abort();

    assert!(
        host_output.status.success(),
        "host failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert!(join_output.status.success());
    assert_eq!(join_output.stdout, payload);

    let join_stderr = String::from_utf8_lossy(&join_output.stderr);
    assert!(
        join_stderr.contains("\"event\":\"complete\""),
        "join stderr did not contain complete event:\n{join_stderr}"
    );
    assert!(
        join_stderr.contains("\"event\":\"offer\""),
        "join stderr did not contain offer event:\n{join_stderr}"
    );
    let host_stderr = String::from_utf8_lossy(&host_output.stderr);
    assert!(
        host_stderr.contains("\"state\":\"Hosting"),
        "first join stderr did not contain hosting state:\n{host_stderr}"
    );
    assert!(
        host_stderr.contains("\"event\":\"waiting\""),
        "first join stderr did not contain waiting event:\n{host_stderr}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_host_to_join_transfers_payload_end_to_end() -> Result<()> {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) if is_socket_permission_error(&error) => return Ok(()),
        Err(error) => return Err(error).context("failed to bind rendezvous test listener"),
    };
    let Some(probe) = bind_transport_or_skip().await else {
        return Ok(());
    };
    probe.close().await;
    let addr = listener
        .local_addr()
        .context("failed to read rendezvous test listener addr")?;
    let server = tokio::spawn(async move {
        let app = build_router(AppState {
            store: Arc::new(InMemoryStreamStore::new()),
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

    let home_dir = fresh_test_dir("skyffla-cli-stdio-missing");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let server_url = format!("http://{addr}");
    let bin = env!("CARGO_BIN_EXE_skyffla");

    wait_for_server_ready(&server_url).await?;

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("--server")
        .arg(&server_url)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    let payload = b"hello from integration test\nsecond line\n";
    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into host stdin")?;
    drop(host_stdin);

    wait_for_stream_ready(&server_url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server_url)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("join process watchdog timed out")?
        .context("join process failed while waiting for output")?;
    if !join_output.status.success() {
        let _ = host.kill().await;
        let host_output = host
            .wait_with_output()
            .await
            .context("host process failed while collecting failure output")?;
        server.abort();
        anyhow::bail!(
            "join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = tokio::time::timeout(PROCESS_TIMEOUT, host.wait_with_output())
        .await
        .context("host process watchdog timed out")?
        .context("host process failed while waiting for output")?;

    server.abort();

    assert!(
        host_output.status.success(),
        "host failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert!(join_output.status.success());
    assert_eq!(join_output.stdout, payload);
    let join_stderr = String::from_utf8_lossy(&join_output.stderr);
    assert!(
        join_stderr.contains("\"event\":\"complete\""),
        "join stderr did not contain complete event:\n{join_stderr}"
    );
    assert!(
        join_stderr.contains("\"event\":\"offer\""),
        "join stderr did not contain offer event:\n{join_stderr}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_mode_transfers_payload_without_rendezvous() -> Result<()> {
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let payload = b"hello from local discovery\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn local host process")?;

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into local host stdin")?;
    drop(host_stdin);

    sleep(LOCAL_DISCOVERY_BOOTSTRAP_DELAY).await;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn local join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("local join process watchdog timed out")?
        .context("local join process failed while waiting for output")?;
    if !join_output.status.success() {
        let _ = host.kill().await;
        let host_output = host
            .wait_with_output()
            .await
            .context("local host process failed while collecting failure output")?;
        anyhow::bail!(
            "local join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = tokio::time::timeout(PROCESS_TIMEOUT, host.wait_with_output())
        .await
        .context("local host process watchdog timed out")?
        .context("local host process failed while waiting for output")?;

    assert!(
        host_output.status.success(),
        "local host failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert_eq!(join_output.stdout, payload);

    let join_stderr = String::from_utf8_lossy(&join_output.stderr);
    assert!(
        !join_stderr.contains("rendezvous_error"),
        "local join unexpectedly referenced rendezvous:\n{join_stderr}"
    );
    assert!(
        !join_stderr.contains("127.0.0.1:9"),
        "local join unexpectedly referenced server URL:\n{join_stderr}"
    );

    Ok(())
}

async fn bind_transport_or_skip() -> Option<IrohTransport> {
    match IrohTransport::bind().await {
        Ok(transport) => Some(transport),
        Err(error) if is_transport_permission_error(&error) => None,
        Err(error) => panic!("iroh bind should succeed: {error}"),
    }
}

async fn local_discovery_available() -> Result<bool> {
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

fn is_socket_permission_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
    ) || error.to_string().contains("Operation not permitted")
}

fn is_transport_permission_error(error: &TransportError) -> bool {
    matches!(error, TransportError::EndpointBind(bind_error) if {
        let message = bind_error.to_string();
        message.contains("Operation not permitted") || message.contains("Failed to bind sockets")
    })
}

fn is_mdns_permission_error(message: &str) -> bool {
    message.contains("Operation not permitted")
        || message.contains("permission denied")
        || message.contains("Addr not available")
}

fn unique_room_name() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    format!("stdio-room-{nonce}")
}

fn fresh_test_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nonce}"))
}

async fn wait_for_server_ready(server_url: &str) -> Result<()> {
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

async fn wait_for_stream_ready(server_url: &str, room: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let stream_url = format!("{server_url}/v1/streams/{room}");
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

    loop {
        if let Ok(response) = client.get(&stream_url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "stream {room} was not registered within {:?}",
                CONNECT_TIMEOUT
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}
