use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use iroh::address_lookup::MdnsAddressLookup;
use skyffla_transport::{IrohTransport, TransportError};

#[allow(dead_code)]
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
#[allow(dead_code)]
pub const LOCAL_DISCOVERY_BOOTSTRAP_DELAY: Duration = Duration::from_millis(500);
#[allow(dead_code)]
pub const LOCAL_JOIN_PROMOTION_DELAY: Duration = Duration::from_secs(2);

pub async fn bind_transport_or_skip() -> Option<IrohTransport> {
    match IrohTransport::bind().await {
        Ok(transport) => Some(transport),
        Err(error) if is_transport_permission_error(&error) => None,
        Err(error) => panic!("iroh bind should succeed: {error}"),
    }
}

pub struct LocalDiscoveryTestGuard(File);

pub fn acquire_local_discovery_test_guard() -> Result<LocalDiscoveryTestGuard> {
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
    Ok(LocalDiscoveryTestGuard(file))
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
        let _ = self.0.unlock();
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
