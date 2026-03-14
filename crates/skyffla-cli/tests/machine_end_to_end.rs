use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

mod support;

use support::{fresh_test_dir, unique_room_name, wait_for_stream_ready, TestServer, PROCESS_TIMEOUT};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_host_to_join_delivers_room_events_and_direct_chat() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-machine");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let bin = env!("CARGO_BIN_EXE_skyffla");

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("host")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    wait_for_stream_ready(&server.url, &room).await?;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("join")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut join = join.spawn().context("failed to spawn join process")?;

    drop(join.stdin.take());

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(
            br#"{"type":"send_chat","to":{"type":"all"},"text":"hello machine room"}"#,
        )
        .await
        .context("failed to write machine command to host stdin")?;
    host_stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate machine command line")?;
    drop(host_stdin);

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("join process watchdog timed out")?
        .context("join process failed while waiting for output")?;
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
    assert!(
        join_output.status.success(),
        "join failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&join_output.stdout),
        String::from_utf8_lossy(&join_output.stderr)
    );

    let host_events = parse_json_lines(&host_output.stdout)?;
    let join_events = parse_json_lines(&join_output.stdout)?;

    assert!(contains_event(&host_events, "room_welcome"));
    assert!(contains_event(&host_events, "member_snapshot"));
    assert!(contains_event(&host_events, "member_joined"));

    assert!(contains_event(&join_events, "room_welcome"));
    assert!(contains_event(&join_events, "member_snapshot"));
    assert!(join_events.iter().any(|event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("host".into()))
            && event.get("text") == Some(&Value::String("hello machine room".into()))
    }));

    Ok(())
}

fn parse_json_lines(output: &[u8]) -> Result<Vec<Value>> {
    let stdout = String::from_utf8(output.to_vec()).context("stdout was not valid utf-8")?;
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).context("failed to parse stdout json line"))
        .collect()
}

fn contains_event(events: &[Value], event_type: &str) -> bool {
    events
        .iter()
        .any(|event| event.get("type") == Some(&Value::String(event_type.into())))
}
