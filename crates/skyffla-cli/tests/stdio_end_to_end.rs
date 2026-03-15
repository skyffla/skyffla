use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

mod support;

use support::{fresh_test_dir, unique_room_name, wait_for_room_ready, TestServer, PROCESS_TIMEOUT};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_join_to_join_transfers_payload_end_to_end() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-stdio");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let payload = b"hello from integration test\nsecond line\n";
    let room = unique_room_name();
    let bin = env!("CARGO_BIN_EXE_skyffla");

    let mut host = Command::new(bin);
    host.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
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

    wait_for_room_ready(&server.url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
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
        join_stderr.contains("\"event\":\"machine_closed\""),
        "join stderr did not contain machine_closed event:\n{join_stderr}"
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
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-stdio-missing");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let bin = env!("CARGO_BIN_EXE_skyffla");

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    wait_for_room_ready(&server.url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn join process")?;

    let payload = b"hello from integration test\nsecond line\n";
    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into host stdin")?;
    drop(host_stdin);

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
        join_stderr.contains("\"event\":\"machine_closed\""),
        "join stderr did not contain machine_closed event:\n{join_stderr}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_rejects_session_mode_mismatch_during_handshake() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-stdio-mode-mismatch");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let bin = env!("CARGO_BIN_EXE_skyffla");

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let host = host.spawn().context("failed to spawn stdio host process")?;

    wait_for_room_ready(&server.url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("join")
        .arg("machine")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join
        .spawn()
        .context("failed to spawn mismatched join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("mismatched join process watchdog timed out")?
        .context("mismatched join process failed while waiting for output")?;
    let host_output = tokio::time::timeout(PROCESS_TIMEOUT, host.wait_with_output())
        .await
        .context("stdio host process watchdog timed out")?
        .context("stdio host process failed while waiting for output")?;

    server.abort();

    assert!(
        !join_output.status.success(),
        "mismatched join unexpectedly succeeded:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&join_output.stdout),
        String::from_utf8_lossy(&join_output.stderr)
    );
    assert!(
        !host_output.status.success(),
        "stdio host unexpectedly succeeded:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    let join_stderr = String::from_utf8_lossy(&join_output.stderr);
    let host_stderr = String::from_utf8_lossy(&host_output.stderr);
    assert!(
        join_stderr.contains("session mode mismatch")
            || join_stderr.contains("peer closed control stream before sending hello"),
        "mismatched join stderr did not mention session mode mismatch or peer closure:\n{join_stderr}"
    );
    assert!(
        host_stderr.contains("session mode mismatch"),
        "stdio host stderr did not mention session mode mismatch:\n{host_stderr}"
    );

    Ok(())
}
