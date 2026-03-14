use std::process::{ExitStatus, Stdio};
use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::sleep;

mod support;

use support::{
    acquire_local_discovery_test_guard, assert_local_mode_stderr, assert_stdio_json_stderr,
    fresh_test_dir, local_discovery_available, unique_room_name, LOCAL_DISCOVERY_BOOTSTRAP_DELAY,
    LOCAL_JOIN_PROMOTION_DELAY, PROCESS_TIMEOUT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_duplex_supports_host_join_promotion_and_simultaneous_join() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    assert_local_host_to_join_duplex().await?;
    assert_local_join_promotion_duplex().await?;
    assert_local_simultaneous_join_duplex().await?;
    Ok(())
}

async fn assert_local_host_to_join_duplex() -> Result<()> {
    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-duplex-host");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let host_payload = b"hello from local host\n";
    let join_payload = b"hello from local join\n";
    let unreachable_server = "http://127.0.0.1:9";

    let mut host = spawn_local_stdio_peer(
        &home_dir,
        "host",
        unreachable_server,
        ["host", room.as_str(), "--name", "host"],
    )?;
    write_child_stdin(&mut host, host_payload, "host stdin").await?;

    sleep(LOCAL_DISCOVERY_BOOTSTRAP_DELAY).await;

    let mut join = spawn_local_stdio_peer(
        &home_dir,
        "join",
        unreachable_server,
        ["join", room.as_str(), "--name", "join"],
    )?;
    write_child_stdin(&mut join, join_payload, "join stdin").await?;

    let join_output = wait_for_output(join, "local join process").await?;
    let host_output = wait_for_output(host, "local host process").await?;

    assert_success(&host_output.status, "local host", &host_output.stdout, &host_output.stderr);
    assert_success(&join_output.status, "local join", &join_output.stdout, &join_output.stderr);
    assert_eq!(host_output.stdout, join_payload);
    assert_eq!(join_output.stdout, host_payload);
    assert_local_mode_stderr(&host_output.stderr);
    assert_local_mode_stderr(&join_output.stderr);
    assert_stdio_json_stderr(&host_output.stderr);
    assert_stdio_json_stderr(&join_output.stderr);

    Ok(())
}

async fn assert_local_join_promotion_duplex() -> Result<()> {
    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-duplex-join");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let first_payload = b"hello from first local join\n";
    let second_payload = b"hello from second local join\n";
    let unreachable_server = "http://127.0.0.1:9";

    let mut first = spawn_local_stdio_peer(
        &home_dir,
        "first local join",
        unreachable_server,
        ["join", room.as_str(), "--name", "first"],
    )?;
    write_child_stdin(&mut first, first_payload, "first local join stdin").await?;

    sleep(LOCAL_JOIN_PROMOTION_DELAY).await;

    let mut second = spawn_local_stdio_peer(
        &home_dir,
        "second local join",
        unreachable_server,
        ["join", room.as_str(), "--name", "second"],
    )?;
    write_child_stdin(&mut second, second_payload, "second local join stdin").await?;

    let second_output = wait_for_output(second, "second local join process").await?;
    let first_output = wait_for_output(first, "first local join process").await?;

    assert_success(
        &first_output.status,
        "first local join",
        &first_output.stdout,
        &first_output.stderr,
    );
    assert_success(
        &second_output.status,
        "second local join",
        &second_output.stdout,
        &second_output.stderr,
    );
    assert_eq!(first_output.stdout, second_payload);
    assert_eq!(second_output.stdout, first_payload);
    assert_local_mode_stderr(&first_output.stderr);
    assert_local_mode_stderr(&second_output.stderr);
    assert_stdio_json_stderr(&first_output.stderr);
    assert_stdio_json_stderr(&second_output.stderr);

    Ok(())
}

async fn assert_local_simultaneous_join_duplex() -> Result<()> {
    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-duplex-race");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let first_payload = b"hello from simultaneous first\n";
    let second_payload = b"hello from simultaneous second\n";
    let unreachable_server = "http://127.0.0.1:9";

    let mut first = spawn_local_stdio_peer(
        &home_dir,
        "first simultaneous local join",
        unreachable_server,
        ["join", room.as_str(), "--name", "first"],
    )?;
    let mut second = spawn_local_stdio_peer(
        &home_dir,
        "second simultaneous local join",
        unreachable_server,
        ["join", room.as_str(), "--name", "second"],
    )?;
    write_child_stdin(&mut first, first_payload, "first simultaneous stdin").await?;
    write_child_stdin(&mut second, second_payload, "second simultaneous stdin").await?;

    let first_output = wait_for_output(first, "first simultaneous local join").await?;
    let second_output = wait_for_output(second, "second simultaneous local join").await?;

    assert_success(
        &first_output.status,
        "first simultaneous local join",
        &first_output.stdout,
        &first_output.stderr,
    );
    assert_success(
        &second_output.status,
        "second simultaneous local join",
        &second_output.stdout,
        &second_output.stderr,
    );
    assert_eq!(first_output.stdout, second_payload);
    assert_eq!(second_output.stdout, first_payload);
    assert_local_mode_stderr(&first_output.stderr);
    assert_local_mode_stderr(&second_output.stderr);
    assert_stdio_json_stderr(&first_output.stderr);
    assert_stdio_json_stderr(&second_output.stderr);

    Ok(())
}

fn spawn_local_stdio_peer<const N: usize>(
    home_dir: &std::path::Path,
    label: &str,
    server_url: &str,
    args: [&str; N],
) -> Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skyffla"));
    command
        .args(args)
        .arg("--local")
        .arg("--server")
        .arg(server_url)
        .arg("--stdio")
        .arg("--json")
        .env("HOME", home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))
}

async fn write_child_stdin(child: &mut Child, payload: &[u8], label: &str) -> Result<()> {
    let mut stdin = child.stdin.take().with_context(|| format!("{label} missing"))?;
    stdin
        .write_all(payload)
        .await
        .with_context(|| format!("failed to write {label}"))?;
    drop(stdin);
    Ok(())
}

async fn wait_for_output(mut child: Child, label: &str) -> Result<std::process::Output> {
    match tokio::time::timeout(PROCESS_TIMEOUT, child.wait()).await {
        Ok(status) => {
            let status = status.with_context(|| format!("{label} failed while waiting for exit"))?;
            collect_child_output(child, status, label).await
        }
        Err(_) => {
            let _ = child.kill().await;
            let status = child
                .wait()
                .await
                .with_context(|| format!("{label} failed while collecting timeout status"))?;
            let output = collect_child_output(child, status, label).await?;
            anyhow::bail!(
                "{label} watchdog timed out\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

async fn collect_child_output(
    mut child: Child,
    status: ExitStatus,
    label: &str,
) -> Result<std::process::Output> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut child_stdout) = child.stdout.take() {
        child_stdout
            .read_to_end(&mut stdout)
            .await
            .with_context(|| format!("failed to read {label} stdout"))?;
    }
    if let Some(mut child_stderr) = child.stderr.take() {
        child_stderr
            .read_to_end(&mut stderr)
            .await
            .with_context(|| format!("failed to read {label} stderr"))?;
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn assert_success(status: &ExitStatus, label: &str, stdout: &[u8], stderr: &[u8]) {
    assert!(
        status.success(),
        "{label} failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
}
