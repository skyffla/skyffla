use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use skyffla_rendezvous::app::{build_router, AppState, IpRateLimiter};
use skyffla_rendezvous::store::InMemoryStreamStore;
use skyffla_transport::{IrohTransport, TransportError};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

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
        });
        axum::serve(listener, app)
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

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into host stdin")?;
    drop(host_stdin);

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

    let host_output = tokio::time::timeout(Duration::from_secs(20), host.wait_with_output())
        .await
        .context("host process timed out")?
        .context("host process failed while waiting for output")?;
    let join_output = tokio::time::timeout(Duration::from_secs(20), join.wait_with_output())
        .await
        .context("join process timed out")?
        .context("join process failed while waiting for output")?;

    server.abort();

    assert!(
        host_output.status.success(),
        "host failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert!(
        join_output.status.success(),
        "join failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&join_output.stdout),
        String::from_utf8_lossy(&join_output.stderr)
    );
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

async fn bind_transport_or_skip() -> Option<IrohTransport> {
    match IrohTransport::bind().await {
        Ok(transport) => Some(transport),
        Err(error) if is_transport_permission_error(&error) => None,
        Err(error) => panic!("iroh bind should succeed: {error}"),
    }
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
