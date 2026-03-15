use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};

mod support;

use support::{fresh_test_dir, unique_room_name, wait_for_room_ready, TestServer};

const TUI_PROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TUI_EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_supports_join_and_broadcast_chat() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let join_home = fresh_test_dir("skyffla-cli-room-tui-join");
    for home in [&host_home, &join_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let room = unique_room_name();
    let mut host = TuiProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = TuiProc::spawn("join", &room, &server.url, "join", &join_home).await?;

    host.expect_line_contains("member joined: join (m2)")
        .await?;
    join.expect_line_contains("joined room").await?;
    join.expect_line_contains("members:").await?;

    host.send_line("hello room").await?;
    host.expect_line_contains("you: hello room").await?;
    join.expect_line_contains("host: hello room").await?;

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_supports_direct_message_command() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let join_home = fresh_test_dir("skyffla-cli-room-tui-join");
    for home in [&host_home, &join_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let room = unique_room_name();
    let mut host = TuiProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = TuiProc::spawn("join", &room, &server.url, "join", &join_home).await?;

    host.expect_line_contains("member joined: join (m2)")
        .await?;
    join.expect_line_contains("joined room").await?;
    join.expect_line_contains("members:").await?;
    join.expect_line_contains("direct room link ready: host (m1)")
        .await?;
    join.send_line("/msg m1 secret hello").await?;
    join.expect_line_contains("you -> host (m1): secret hello")
        .await?;
    host.expect_line_contains("join -> m1: secret hello")
        .await?;

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_supports_file_send_default_accept_and_save() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let join_home = fresh_test_dir("skyffla-cli-room-tui-join");
    for home in [&host_home, &join_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }
    std::fs::write(join_home.join("report.txt"), b"mesh report")?;

    let room = unique_room_name();
    let mut host = TuiProc::spawn("host", &room, &server.url, "alpha", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = TuiProc::spawn("join", &room, &server.url, "beta", &join_home).await?;

    host.expect_line_contains("member joined: beta (m2)").await?;
    join.expect_line_contains("joined room").await?;
    join.expect_line_contains("members:").await?;
    join.expect_line_contains("direct room link ready: alpha (m1)")
        .await?;

    join.send_line(r#"/send m1 ~/report.txt"#).await?;
    join.expect_line_contains("sending file report.txt to alpha (m1)")
        .await?;
    host.expect_line_contains("beta wants to send file report.txt (11B) - /accept or /reject")
        .await?;

    host.send_line("/accept").await?;
    host.expect_line_contains("accepting file report.txt 11B")
        .await?;
    host.expect_line_contains("downloading file (report.txt)")
        .await?;
    host.expect_line_contains("saving file report.txt 11B to report.txt")
        .await?;
    host.expect_line_contains("file report.txt 11B saved to report.txt")
        .await?;

    assert_eq!(std::fs::read(host_home.join("report.txt"))?, b"mesh report");

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_supports_folder_send_with_progress() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let join_home = fresh_test_dir("skyffla-cli-room-tui-join");
    for home in [&host_home, &join_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }
    let source_dir = join_home.join("artpack");
    std::fs::create_dir_all(source_dir.join("nested"))?;
    std::fs::write(source_dir.join("a.txt"), b"alpha")?;
    std::fs::write(source_dir.join("nested").join("b.txt"), b"beta")?;

    let room = unique_room_name();
    let mut host = TuiProc::spawn("host", &room, &server.url, "alpha", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = TuiProc::spawn("join", &room, &server.url, "beta", &join_home).await?;

    host.expect_line_contains("member joined: beta (m2)").await?;
    join.expect_line_contains("joined room").await?;
    join.expect_line_contains("members:").await?;
    join.expect_line_contains("direct room link ready: alpha (m1)")
        .await?;
    join.send_line(r#"/send m1 ~/artpack"#).await?;
    join.expect_line_contains("sending folder artpack to alpha (m1)")
        .await?;
    host.expect_line_contains("beta wants to send folder artpack (9B) - /accept or /reject")
        .await?;

    host.send_line("/accept").await?;
    host.expect_line_contains("downloading folder (artpack)").await?;
    host.expect_line_contains("saving folder artpack 9B to artpack")
        .await?;
    host.expect_line_contains("folder artpack 9B saved to artpack")
        .await?;

    assert_eq!(std::fs::read(host_home.join("artpack").join("a.txt"))?, b"alpha");
    assert_eq!(
        std::fs::read(host_home.join("artpack").join("nested").join("b.txt"))?,
        b"beta"
    );

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_hides_targeted_file_transfer_from_third_member() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let beta_home = fresh_test_dir("skyffla-cli-room-tui-beta");
    let gamma_home = fresh_test_dir("skyffla-cli-room-tui-gamma");
    for home in [&host_home, &beta_home, &gamma_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }
    std::fs::write(beta_home.join("secret.txt"), b"secret")?;

    let room = unique_room_name();
    let mut alpha = TuiProc::spawn("host", &room, &server.url, "alpha", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = TuiProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = TuiProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    alpha.expect_line_contains("member joined: beta").await?;
    alpha.expect_line_contains("member joined: gamma").await?;
    beta.expect_line_contains("members:").await?;
    gamma.expect_line_contains("members:").await?;

    beta.send_line(r#"/send alpha ~/secret.txt"#).await?;
    beta.expect_line_contains("sending file secret.txt to alpha (m1)")
        .await?;
    alpha.expect_line_contains("beta wants to send file secret.txt (6B) - /accept or /reject")
        .await?;

    alpha.send_line("/accept").await?;
    alpha.expect_line_contains("file secret.txt 6B saved to secret.txt")
        .await?;

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let gamma_dump = gamma.debug_dump().await;
    assert!(
        !gamma_dump.contains("secret.txt"),
        "gamma unexpectedly saw targeted file transfer:\n{gamma_dump}"
    );

    alpha.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_announces_when_member_leaves() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let alpha_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let beta_home = fresh_test_dir("skyffla-cli-room-tui-beta");
    let gamma_home = fresh_test_dir("skyffla-cli-room-tui-gamma");
    for home in [&alpha_home, &beta_home, &gamma_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let room = unique_room_name();
    let mut alpha = TuiProc::spawn("host", &room, &server.url, "alpha", &alpha_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = TuiProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = TuiProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    alpha.expect_line_contains("member joined: beta").await?;
    alpha.expect_line_contains("member joined: gamma").await?;
    beta.expect_line_contains("members:").await?;
    gamma.expect_line_contains("members:").await?;

    beta.send_line("/quit").await?;
    alpha.expect_line_contains("member left: beta")
        .await?;
    gamma.expect_line_contains("member left: beta")
        .await?;

    alpha.shutdown().await?;
    gamma.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_announces_when_host_leaves() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let alpha_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let beta_home = fresh_test_dir("skyffla-cli-room-tui-beta");
    for home in [&alpha_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let room = unique_room_name();
    let mut alpha = TuiProc::spawn("host", &room, &server.url, "alpha", &alpha_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = TuiProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    alpha.expect_line_contains("member joined: beta (m2)").await?;
    beta.expect_line_contains("joined room").await?;

    alpha.send_line("/quit").await?;
    beta.expect_line_contains("room closed: host left").await?;

    beta.shutdown().await?;
    alpha.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_tui_auto_save_appends_suffix_on_name_collision() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-room-tui-host");
    let join_home = fresh_test_dir("skyffla-cli-room-tui-join");
    for home in [&host_home, &join_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }
    std::fs::write(host_home.join("report.txt"), b"existing report")?;
    std::fs::write(join_home.join("report.txt"), b"mesh report")?;

    let room = unique_room_name();
    let mut host = TuiProc::spawn("host", &room, &server.url, "alpha", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = TuiProc::spawn("join", &room, &server.url, "beta", &join_home).await?;

    host.expect_line_contains("member joined: beta (m2)").await?;
    join.expect_line_contains("direct room link ready: alpha (m1)")
        .await?;

    join.send_line(r#"/send m1 ~/report.txt"#).await?;
    join.expect_line_contains("sending file report.txt to alpha (m1)")
        .await?;
    host.expect_line_contains("beta wants to send file report.txt (11B) - /accept or /reject")
        .await?;

    host.send_line("/accept").await?;
    host.expect_line_contains("saving file report.txt 11B to report (1).txt")
        .await?;
    host.expect_line_contains("file report.txt 11B saved to report (1).txt")
        .await?;

    assert_eq!(std::fs::read(host_home.join("report.txt"))?, b"existing report");
    assert_eq!(std::fs::read(host_home.join("report (1).txt"))?, b"mesh report");

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
    Ok(())
}

struct TuiProc {
    label: String,
    child: Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout_rx: mpsc::UnboundedReceiver<String>,
    stdout_seen: Arc<Mutex<Vec<String>>>,
}

impl TuiProc {
    async fn spawn(
        role: &str,
        room: &str,
        server_url: &str,
        name: &str,
        home: &Path,
    ) -> Result<Self> {
        let bin = env!("CARGO_BIN_EXE_skyffla");
        let mut command = Command::new(bin);
        command
            .arg(role)
            .arg(room)
            .arg("--server")
            .arg(server_url)
            .arg("--name")
            .arg(name)
            .current_dir(home)
            .env("HOME", home)
            .env("SKYFFLA_TUI_SCRIPTED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {role} process"))?;
        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout = child.stdout.take().context("child stdout missing")?;
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel();
        let stdout_seen = Arc::new(Mutex::new(Vec::new()));
        spawn_stdout_reader(stdout, stdout_tx, stdout_seen.clone());

        Ok(Self {
            label: format!("{role}:{name}"),
            child,
            stdin: Some(stdin),
            stdout_rx,
            stdout_seen,
        })
    }

    async fn send_line(&mut self, line: &str) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("{} stdin already closed", self.label))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("failed writing line to {}", self.label))?;
        stdin
            .write_all(b"\n")
            .await
            .with_context(|| format!("failed terminating line for {}", self.label))?;
        stdin
            .flush()
            .await
            .with_context(|| format!("failed flushing stdin for {}", self.label))?;
        Ok(())
    }

    async fn expect_line_contains(&mut self, needle: &str) -> Result<String> {
        let deadline = Instant::now() + TUI_EVENT_TIMEOUT;
        loop {
            {
                let seen = self.stdout_seen.lock().await;
                if let Some(found) = seen.iter().find(|line| line.contains(needle)).cloned() {
                    return Ok(found);
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!(
                    "timed out waiting for {needle} on {}\nstdout:\n{}",
                    self.label,
                    self.debug_dump().await
                );
            }

            match tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(250)),
                self.stdout_rx.recv(),
            )
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => {
                    anyhow::bail!(
                        "{} stdout closed while waiting for {needle}\nstdout:\n{}",
                        self.label,
                        self.debug_dump().await
                    );
                }
                Err(_) => {}
            }
        }
    }

    async fn debug_dump(&self) -> String {
        self.stdout_seen.lock().await.join("\n")
    }

    async fn shutdown(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"/quit\n").await;
            let _ = stdin.flush().await;
        }
        if tokio::time::timeout(TUI_PROCESS_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        Ok(())
    }
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    tx: mpsc::UnboundedSender<String>,
    seen: Arc<Mutex<Vec<String>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            seen.lock().await.push(line.clone());
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}
