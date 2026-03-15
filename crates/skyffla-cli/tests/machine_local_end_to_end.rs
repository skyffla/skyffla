use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

mod support;

use support::{
    acquire_local_discovery_test_guard, fresh_test_dir, local_discovery_available,
    unique_room_name, LOCAL_DISCOVERY_BOOTSTRAP_DELAY, LOCAL_JOIN_PROMOTION_DELAY, PROCESS_TIMEOUT,
};

const UNREACHABLE_SERVER: &str = "http://127.0.0.1:9";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_local_host_to_join_delivers_room_events_and_chat() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-machine-local");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn_local("host", &room, "host", &home_dir).await?;
    sleep(LOCAL_DISCOVERY_BOOTSTRAP_DELAY).await;
    let mut join = MachineProc::spawn_local("join", &room, "join", &home_dir).await?;

    host.expect_event("member_joined", |event| {
        event.get("type") == Some(&Value::String("member_joined".into()))
            && event.pointer("/member/name") == Some(&Value::String("join".into()))
    })
    .await?;
    join.expect_event("member snapshot with host+join", |event| {
        event.get("type") == Some(&Value::String("member_snapshot".into()))
            && member_count(event) == Some(2)
    })
    .await?;
    host.expect_stderr_contains("\"member_name\":\"join\"")
        .await?;
    join.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello local machine room"}"#)
        .await?;
    join.expect_event("host chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("host".into()))
            && event.get("text") == Some(&Value::String("hello local machine room".into()))
    })
    .await?;

    join.send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello back from join"}"#)
        .await?;
    host.expect_event("join chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("join".into()))
            && event.get("text") == Some(&Value::String("hello back from join".into()))
    })
    .await?;

    host.shutdown().await?;
    join.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_local_two_joins_connect_after_promotion_and_chat() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-machine-local-join");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let mut alpha = MachineProc::spawn_local("join", &room, "alpha", &home_dir).await?;
    alpha
        .expect_event("initial self snapshot", |event| {
            event.get("type") == Some(&Value::String("member_snapshot".into()))
                && member_count(event) == Some(1)
        })
        .await?;

    sleep(LOCAL_JOIN_PROMOTION_DELAY).await;

    let mut beta = MachineProc::spawn_local("join", &room, "beta", &home_dir).await?;
    alpha
        .expect_event("beta joined", |event| {
            event.get("type") == Some(&Value::String("member_joined".into()))
                && event.pointer("/member/name") == Some(&Value::String("beta".into()))
        })
        .await?;
    beta.expect_event("member snapshot with alpha+beta", |event| {
        event.get("type") == Some(&Value::String("member_snapshot".into()))
            && member_count(event) == Some(2)
    })
    .await?;
    alpha
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"alpha\"")
        .await?;

    alpha
        .send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello from promoted join"}"#)
        .await?;
    beta.expect_event("alpha chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("alpha".into()))
            && event.get("text") == Some(&Value::String("hello from promoted join".into()))
    })
    .await?;

    beta.send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello from second join"}"#)
        .await?;
    alpha
        .expect_event("beta chat", |event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("from_name") == Some(&Value::String("beta".into()))
                && event.get("text") == Some(&Value::String("hello from second join".into()))
        })
        .await?;

    alpha.shutdown().await?;
    beta.shutdown().await?;
    Ok(())
}

struct MachineProc {
    label: String,
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_rx: mpsc::UnboundedReceiver<Value>,
    stderr_rx: mpsc::UnboundedReceiver<String>,
    stdout_seen: Arc<Mutex<Vec<Value>>>,
    stderr_seen: Arc<Mutex<Vec<String>>>,
}

impl MachineProc {
    async fn spawn_local(role: &str, room: &str, name: &str, home: &Path) -> Result<Self> {
        let bin = env!("CARGO_BIN_EXE_skyffla");
        let mut command = Command::new(bin);
        command
            .arg(role)
            .arg(room)
            .arg("machine")
            .arg("--server")
            .arg(UNREACHABLE_SERVER)
            .arg("--name")
            .arg(name)
            .arg("--json")
            .arg("--local")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn local {role} process"))?;
        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;

        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel();
        let stdout_seen = Arc::new(Mutex::new(Vec::new()));
        let stderr_seen = Arc::new(Mutex::new(Vec::new()));
        spawn_stdout_reader(stdout, stdout_tx, stdout_seen.clone());
        spawn_stderr_reader(stderr, stderr_tx, stderr_seen.clone());

        Ok(Self {
            label: format!("{role}:{name}"),
            child,
            stdin: Some(stdin),
            stdout_rx,
            stderr_rx,
            stdout_seen,
            stderr_seen,
        })
    }

    async fn send(&mut self, line: &str) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("{} stdin already closed", self.label))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("failed writing command to {}", self.label))?;
        stdin
            .write_all(b"\n")
            .await
            .with_context(|| format!("failed terminating command for {}", self.label))?;
        stdin
            .flush()
            .await
            .with_context(|| format!("failed flushing stdin for {}", self.label))?;
        Ok(())
    }

    async fn expect_event<F>(&mut self, label: &str, predicate: F) -> Result<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Some(found) = self
                .stdout_seen
                .lock()
                .await
                .iter()
                .find(|event| predicate(event))
                .cloned()
            {
                return Ok(found);
            }

            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                bail!(
                    "timed out waiting for {} on {}\n{}",
                    label,
                    self.label,
                    self.debug_dump().await
                );
            };

            match tokio::time::timeout(timeout, self.stdout_rx.recv()).await {
                Ok(Some(_)) => {}
                Ok(None) => bail!(
                    "stdout closed while waiting for {} on {}\n{}",
                    label,
                    self.label,
                    self.debug_dump().await
                ),
                Err(_) => {
                    bail!(
                        "timed out waiting for {} on {}\n{}",
                        label,
                        self.label,
                        self.debug_dump().await
                    )
                }
            }
        }
    }

    async fn expect_stderr_contains(&mut self, needle: &str) -> Result<()> {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if self
                .stderr_seen
                .lock()
                .await
                .iter()
                .any(|line| line.contains(needle))
            {
                return Ok(());
            }

            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                bail!(
                    "timed out waiting for stderr containing {:?} on {}\n{}",
                    needle,
                    self.label,
                    self.debug_dump().await
                );
            };

            match tokio::time::timeout(timeout, self.stderr_rx.recv()).await {
                Ok(Some(_)) => {}
                Ok(None) => bail!(
                    "stderr closed while waiting for {:?} on {}\n{}",
                    needle,
                    self.label,
                    self.debug_dump().await
                ),
                Err(_) => {
                    bail!(
                        "timed out waiting for stderr containing {:?} on {}\n{}",
                        needle,
                        self.label,
                        self.debug_dump().await
                    )
                }
            }
        }
    }

    async fn debug_dump(&self) -> String {
        let stdout = self
            .stdout_seen
            .lock()
            .await
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let stderr = self.stderr_seen.lock().await.join("\n");
        format!("stdout:\n{stdout}\n\nstderr:\n{stderr}")
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
        }

        if self.child.try_wait()?.is_none() {
            self.child
                .start_kill()
                .with_context(|| format!("failed to kill {}", self.label))?;
            let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        }

        Ok(())
    }
}

impl Drop for MachineProc {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    tx: mpsc::UnboundedSender<Value>,
    seen: Arc<Mutex<Vec<Value>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                seen.lock().await.push(value.clone());
                if tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_reader(
    stderr: ChildStderr,
    tx: mpsc::UnboundedSender<String>,
    seen: Arc<Mutex<Vec<String>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            seen.lock().await.push(line.clone());
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn member_count(event: &Value) -> Option<usize> {
    event.get("members").and_then(Value::as_array).map(Vec::len)
}
