use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use iroh::address_lookup::MdnsAddressLookup;
use skyffla_transport::{IrohTransport, TransportError};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::sleep;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const LOCAL_DISCOVERY_BOOTSTRAP_DELAY: Duration = Duration::from_millis(500);
const LOCAL_JOIN_PROMOTION_DELAY: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_host_to_join_transfers_payload_without_rendezvous() -> Result<()> {
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-host");
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
    assert_local_mode_stderr(&join_output.stderr);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_join_to_join_promotes_first_peer_without_rendezvous() -> Result<()> {
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-join");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let payload = b"hello from local join discovery\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut first_join = Command::new(bin);
    first_join
        .arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("first")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut first_join = first_join
        .spawn()
        .context("failed to spawn first local join process")?;

    let mut first_stdin = first_join
        .stdin
        .take()
        .context("first join stdin missing")?;
    first_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into first local join stdin")?;
    drop(first_stdin);

    sleep(LOCAL_JOIN_PROMOTION_DELAY).await;

    let mut second_join = Command::new(bin);
    second_join
        .arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("second")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let second_join = second_join
        .spawn()
        .context("failed to spawn second local join process")?;

    let second_output = tokio::time::timeout(PROCESS_TIMEOUT, second_join.wait_with_output())
        .await
        .context("second local join process watchdog timed out")?
        .context("second local join process failed while waiting for output")?;
    if !second_output.status.success() {
        let _ = first_join.kill().await;
        let first_output = first_join
            .wait_with_output()
            .await
            .context("first local join process failed while collecting failure output")?;
        anyhow::bail!(
            "second local join failed:\nstdout={}\nstderr={}\nfirst stdout={}\nfirst stderr={}",
            String::from_utf8_lossy(&second_output.stdout),
            String::from_utf8_lossy(&second_output.stderr),
            String::from_utf8_lossy(&first_output.stdout),
            String::from_utf8_lossy(&first_output.stderr)
        );
    }
    let first_output = tokio::time::timeout(PROCESS_TIMEOUT, first_join.wait_with_output())
        .await
        .context("first local join process watchdog timed out")?
        .context("first local join process failed while waiting for output")?;

    assert!(
        first_output.status.success(),
        "first local join failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first_output.stdout),
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert_eq!(second_output.stdout, payload);
    assert_local_mode_stderr(&second_output.stderr);

    Ok(())
}

fn assert_local_mode_stderr(stderr: &[u8]) {
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
    format!("local-room-{nonce}")
}

fn fresh_test_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nonce}"))
}
