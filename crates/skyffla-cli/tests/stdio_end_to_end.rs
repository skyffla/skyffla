use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

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

    let mut host = spawn_stdio_peer(
        bin,
        &home_dir,
        "host process",
        [
            "join",
            room.as_str(),
            "--server",
            server.url.as_str(),
            "--name",
            "host",
            "--stdio",
            "--json",
        ],
    )?;

    wait_for_room_ready(&server.url, &room).await?;

    let join = spawn_stdio_peer(
        bin,
        &home_dir,
        "join process",
        [
            "join",
            room.as_str(),
            "--server",
            server.url.as_str(),
            "--name",
            "join",
            "--stdio",
            "--json",
        ],
    )?;
    host.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    join.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    host.write_stdin_and_close(payload, "host stdin").await?;

    let join_output = join.wait_with_output("join process").await?;
    if !join_output.status.success() {
        let host_output = host.kill_and_collect("host process").await?;
        server.abort();
        anyhow::bail!(
            "join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = host.wait_with_output("host process").await?;

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

    let mut host = spawn_stdio_peer(
        bin,
        &home_dir,
        "host process",
        [
            "host",
            room.as_str(),
            "--server",
            server.url.as_str(),
            "--name",
            "host",
            "--stdio",
            "--json",
        ],
    )?;

    wait_for_room_ready(&server.url, &room).await?;

    let join = spawn_stdio_peer(
        bin,
        &home_dir,
        "join process",
        [
            "join",
            room.as_str(),
            "--server",
            server.url.as_str(),
            "--name",
            "join",
            "--stdio",
            "--json",
        ],
    )?;

    let payload = b"hello from integration test\nsecond line\n";
    host.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    join.wait_for_stderr_contains("\"event\":\"connection_status\"")
        .await?;
    host.write_stdin_and_close(payload, "host stdin").await?;

    let join_output = join.wait_with_output("join process").await?;
    if !join_output.status.success() {
        let host_output = host.kill_and_collect("host process").await?;
        server.abort();
        anyhow::bail!(
            "join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = host.wait_with_output("host process").await?;

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

fn spawn_stdio_peer<const N: usize>(
    bin: &str,
    home_dir: &std::path::Path,
    label: &str,
    args: [&str; N],
) -> Result<SpawnedPeer> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .env("HOME", home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;
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

    async fn wait_with_output(mut self, label: &str) -> Result<std::process::Output> {
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

    async fn kill_and_collect(mut self, label: &str) -> Result<std::process::Output> {
        let _ = self.child.kill().await;
        self.wait_with_output(label).await
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
