use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

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

    wait_for_room_ready(&server.url, &room).await?;

    let mut join = spawn_stdio_peer(
        &home_dir,
        "join",
        &server.url,
        ["join", room.as_str(), "--name", "join"],
    )?;
    host.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    join.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    host.write_stdin_and_close(host_payload, "host stdin")
        .await?;
    join.write_stdin_and_close(join_payload, "join stdin")
        .await?;

    let join_output = wait_for_output(join, "join process").await?;
    let host_output = wait_for_output(host, "host process").await?;

    server.abort();

    assert_success(
        &host_output.status,
        "host",
        &host_output.stdout,
        &host_output.stderr,
    );
    assert_success(
        &join_output.status,
        "join",
        &join_output.stdout,
        &join_output.stderr,
    );
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

    wait_for_room_ready(&server.url, &room).await?;

    let mut second = spawn_stdio_peer(
        &home_dir,
        "second",
        &server.url,
        ["join", room.as_str(), "--name", "second"],
    )?;
    // Wait until both peers report an established connection before sending bytes.
    // The promoted-join host path can still be finalizing after the stdio state transition,
    // so using the later connection_status event removes that race without blocking startup.
    first
        .wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    second
        .wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    first
        .write_stdin_and_close(first_payload, "first join stdin")
        .await?;
    second
        .write_stdin_and_close(second_payload, "second join stdin")
        .await?;

    let second_output = wait_for_output(second, "second join process").await?;
    let first_output = wait_for_output(first, "first join process").await?;

    server.abort();

    assert_success(
        &first_output.status,
        "first join",
        &first_output.stdout,
        &first_output.stderr,
    );
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
) -> Result<SpawnedPeer> {
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
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {label} process"))?;
    let stderr = child
        .stderr
        .take()
        .with_context(|| format!("{label} stderr missing"))?;
    let stderr_seen = Arc::new(Mutex::new(String::new()));
    let stderr_task = spawn_stderr_reader(stderr, stderr_seen.clone());
    Ok(SpawnedPeer {
        child,
        stderr_seen,
        stderr_task,
    })
}

async fn wait_for_output(peer: SpawnedPeer, label: &str) -> Result<std::process::Output> {
    peer.wait_for_output(label).await
}

fn assert_success(status: &ExitStatus, label: &str, stdout: &[u8], stderr: &[u8]) {
    assert!(
        status.success(),
        "{label} failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
}

struct SpawnedPeer {
    child: Child,
    stderr_seen: Arc<Mutex<String>>,
    stderr_task: JoinHandle<Result<()>>,
}

impl SpawnedPeer {
    async fn write_stdin_and_close(&mut self, payload: &[u8], label: &str) -> Result<()> {
        let mut stdin = self
            .child
            .stdin
            .take()
            .with_context(|| format!("{label} missing"))?;
        stdin
            .write_all(payload)
            .await
            .with_context(|| format!("failed to write {label}"))?;
        drop(stdin);
        Ok(())
    }

    async fn wait_for_stderr_contains(&self, needle: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + PROCESS_TIMEOUT;
        loop {
            {
                let stderr = self.stderr_seen.lock().await;
                if stderr.contains(needle) {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let stderr = self.stderr_seen.lock().await.clone();
                anyhow::bail!("timed out waiting for {needle} in stderr:\n{stderr}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn wait_for_output(mut self, label: &str) -> Result<std::process::Output> {
        match tokio::time::timeout(PROCESS_TIMEOUT, self.child.wait()).await {
            Ok(status) => {
                let status =
                    status.with_context(|| format!("{label} failed while waiting for exit"))?;
                let mut stdout = Vec::new();
                if let Some(mut child_stdout) = self.child.stdout.take() {
                    child_stdout
                        .read_to_end(&mut stdout)
                        .await
                        .with_context(|| format!("failed to read {label} stdout"))?;
                }
                self.stderr_task
                    .await
                    .context("stderr reader task panicked")??;
                let stderr = self.stderr_seen.lock().await.clone().into_bytes();
                Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                })
            }
            Err(_) => {
                let _ = self.child.kill().await;
                let _status =
                    self.child.wait().await.with_context(|| {
                        format!("{label} failed while collecting timeout status")
                    })?;
                let mut stdout = Vec::new();
                if let Some(mut child_stdout) = self.child.stdout.take() {
                    child_stdout
                        .read_to_end(&mut stdout)
                        .await
                        .with_context(|| format!("failed to read {label} stdout"))?;
                }
                self.stderr_task
                    .await
                    .context("stderr reader task panicked")??;
                let stderr = self.stderr_seen.lock().await.clone().into_bytes();
                anyhow::bail!(
                    "{label} watchdog timed out\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    }
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    stderr_seen: Arc<Mutex<String>>,
) -> JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .context("failed to read child stderr")?
        {
            let mut seen = stderr_seen.lock().await;
            seen.push_str(&line);
            seen.push('\n');
        }
        Ok(())
    })
}
