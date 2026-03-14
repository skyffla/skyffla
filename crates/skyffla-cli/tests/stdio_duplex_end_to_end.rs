use std::process::{ExitStatus, Stdio};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

mod support;

use support::{
    assert_stdio_json_stderr, fresh_test_dir, unique_room_name, wait_for_room_ready, TestServer,
    PROCESS_TIMEOUT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_host_to_join_streams_bytes_both_directions_end_to_end() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-stdio-duplex-host");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let host_payload = b"hello from host\n";
    let join_payload = b"hello from join\n";

    let mut host = spawn_stdio_peer(
        &home_dir,
        "host",
        &server.url,
        ["host", room.as_str(), "--name", "host"],
    )?;
    write_child_stdin(&mut host, host_payload, "host stdin").await?;

    wait_for_room_ready(&server.url, &room).await?;

    let mut join = spawn_stdio_peer(
        &home_dir,
        "join",
        &server.url,
        ["join", room.as_str(), "--name", "join"],
    )?;
    write_child_stdin(&mut join, join_payload, "join stdin").await?;

    let join_output = wait_for_output(join, "join process").await?;
    let host_output = wait_for_output(host, "host process").await?;

    server.abort();

    assert_success(&host_output.status, "host", &host_output.stdout, &host_output.stderr);
    assert_success(&join_output.status, "join", &join_output.stdout, &join_output.stderr);
    assert_eq!(host_output.stdout, join_payload);
    assert_eq!(join_output.stdout, host_payload);
    assert_stdio_json_stderr(&host_output.stderr);
    assert_stdio_json_stderr(&join_output.stderr);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_join_to_join_streams_bytes_both_directions_end_to_end() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-stdio-duplex-join");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let first_payload = b"hello from first join\n";
    let second_payload = b"hello from second join\n";

    let mut first = spawn_stdio_peer(
        &home_dir,
        "first",
        &server.url,
        ["join", room.as_str(), "--name", "first"],
    )?;
    write_child_stdin(&mut first, first_payload, "first join stdin").await?;

    wait_for_room_ready(&server.url, &room).await?;

    let mut second = spawn_stdio_peer(
        &home_dir,
        "second",
        &server.url,
        ["join", room.as_str(), "--name", "second"],
    )?;
    write_child_stdin(&mut second, second_payload, "second join stdin").await?;

    let second_output = wait_for_output(second, "second join process").await?;
    let first_output = wait_for_output(first, "first join process").await?;

    server.abort();

    assert_success(&first_output.status, "first join", &first_output.stdout, &first_output.stderr);
    assert_success(
        &second_output.status,
        "second join",
        &second_output.stdout,
        &second_output.stderr,
    );
    assert_eq!(first_output.stdout, second_payload);
    assert_eq!(second_output.stdout, first_payload);
    assert_stdio_json_stderr(&first_output.stderr);
    assert_stdio_json_stderr(&second_output.stderr);

    Ok(())
}

fn spawn_stdio_peer<const N: usize>(
    home_dir: &std::path::Path,
    label: &str,
    server_url: &str,
    args: [&str; N],
) -> Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skyffla"));
    command
        .args(args)
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
        .with_context(|| format!("failed to spawn {label} process"))
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

async fn wait_for_output(child: Child, label: &str) -> Result<std::process::Output> {
    tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .with_context(|| format!("{label} watchdog timed out"))?
        .with_context(|| format!("{label} failed while waiting for output"))
}

fn assert_success(status: &ExitStatus, label: &str, stdout: &[u8], stderr: &[u8]) {
    assert!(
        status.success(),
        "{label} failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
}
