use std::process::Stdio;
use std::time::Duration;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_host_accepts_two_joiners_and_broadcasts_to_both() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    let gamma_home = fresh_test_dir("skyffla-cli-machine-gamma");
    for home in [&host_home, &beta_home, &gamma_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

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
        .env("HOME", &host_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    wait_for_stream_ready(&server.url, &room).await?;

    let mut beta = Command::new(bin);
    beta.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("beta")
        .arg("--json")
        .env("HOME", &beta_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut beta = beta.spawn().context("failed to spawn beta process")?;
    drop(beta.stdin.take());

    let mut gamma = Command::new(bin);
    gamma.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("gamma")
        .arg("--json")
        .env("HOME", &gamma_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut gamma = gamma.spawn().context("failed to spawn gamma process")?;
    drop(gamma.stdin.take());

    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(br#"{"type":"send_chat","to":{"type":"all"},"text":"hello everyone"}"#)
        .await
        .context("failed to write machine broadcast to host stdin")?;
    host_stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate host machine command line")?;
    drop(host_stdin);

    let beta_output = tokio::time::timeout(PROCESS_TIMEOUT, beta.wait_with_output())
        .await
        .context("beta process watchdog timed out")?
        .context("beta process failed while waiting for output")?;
    let gamma_output = tokio::time::timeout(PROCESS_TIMEOUT, gamma.wait_with_output())
        .await
        .context("gamma process watchdog timed out")?
        .context("gamma process failed while waiting for output")?;
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
        beta_output.status.success(),
        "beta failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&beta_output.stdout),
        String::from_utf8_lossy(&beta_output.stderr)
    );
    assert!(
        gamma_output.status.success(),
        "gamma failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&gamma_output.stdout),
        String::from_utf8_lossy(&gamma_output.stderr)
    );

    let beta_events = parse_json_lines(&beta_output.stdout)?;
    let gamma_events = parse_json_lines(&gamma_output.stdout)?;

    assert!(beta_events.iter().any(|event| {
        event.get("type") == Some(&Value::String("member_joined".into()))
            && event.pointer("/member/name") == Some(&Value::String("gamma".into()))
    }));
    assert!(gamma_events.iter().any(|event| {
        event.get("type") == Some(&Value::String("member_snapshot".into()))
            && event
                .get("members")
                .and_then(Value::as_array)
                .is_some_and(|members| members.len() == 3)
    }));
    for events in [&beta_events, &gamma_events] {
        assert!(events.iter().any(|event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("from_name") == Some(&Value::String("host".into()))
                && event.get("text") == Some(&Value::String("hello everyone".into()))
        }));
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_joiner_can_chat_directly_to_another_joiner() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    let gamma_home = fresh_test_dir("skyffla-cli-machine-gamma");
    for home in [&host_home, &beta_home, &gamma_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

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
        .env("HOME", &host_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    wait_for_stream_ready(&server.url, &room).await?;

    let mut beta = Command::new(bin);
    beta.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("beta")
        .arg("--json")
        .env("HOME", &beta_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut beta = beta.spawn().context("failed to spawn beta process")?;

    let mut gamma = Command::new(bin);
    gamma.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("gamma")
        .arg("--json")
        .env("HOME", &gamma_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut gamma = gamma.spawn().context("failed to spawn gamma process")?;
    drop(gamma.stdin.take());

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut beta_stdin = beta.stdin.take().context("beta stdin missing")?;
    beta_stdin
        .write_all(br#"{"type":"send_chat","to":{"type":"member","member_id":"m3"},"text":"secret hello"}"#)
        .await
        .context("failed to write beta direct chat command")?;
    beta_stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate beta machine command line")?;
    drop(beta_stdin);

    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(host.stdin.take());

    let beta_output = tokio::time::timeout(PROCESS_TIMEOUT, beta.wait_with_output())
        .await
        .context("beta process watchdog timed out")?
        .context("beta process failed while waiting for output")?;
    let gamma_output = tokio::time::timeout(PROCESS_TIMEOUT, gamma.wait_with_output())
        .await
        .context("gamma process watchdog timed out")?
        .context("gamma process failed while waiting for output")?;
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
        beta_output.status.success(),
        "beta failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&beta_output.stdout),
        String::from_utf8_lossy(&beta_output.stderr)
    );
    assert!(
        gamma_output.status.success(),
        "gamma failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&gamma_output.stdout),
        String::from_utf8_lossy(&gamma_output.stderr)
    );

    let host_events = parse_json_lines(&host_output.stdout)?;
    let gamma_events = parse_json_lines(&gamma_output.stdout)?;
    let beta_stderr = String::from_utf8_lossy(&beta_output.stderr);
    let gamma_stderr = String::from_utf8_lossy(&gamma_output.stderr);

    assert!(
        !host_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("text") == Some(&Value::String("secret hello".into()))
        }),
        "host unexpectedly saw direct beta->gamma chat:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert!(gamma_events.iter().any(|event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("beta".into()))
            && event.get("text") == Some(&Value::String("secret hello".into()))
    }));
    assert!(
        beta_stderr.contains("\"event\":\"room_link_connected\""),
        "beta stderr did not show peer link setup:\n{beta_stderr}"
    );
    assert!(
        gamma_stderr.contains("\"event\":\"room_link_connected\""),
        "gamma stderr did not show peer link setup:\n{gamma_stderr}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_joiner_can_chat_directly_to_host_member() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

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
        .env("HOME", &host_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn host process")?;

    wait_for_stream_ready(&server.url, &room).await?;

    let mut beta = Command::new(bin);
    beta.arg("join")
        .arg(&room)
        .arg("machine")
        .arg("--server")
        .arg(&server.url)
        .arg("--name")
        .arg("beta")
        .arg("--json")
        .env("HOME", &beta_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut beta = beta.spawn().context("failed to spawn beta process")?;

    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut beta_stdin = beta.stdin.take().context("beta stdin missing")?;
    beta_stdin
        .write_all(br#"{"type":"send_chat","to":{"type":"member","member_id":"m1"},"text":"hi host"}"#)
        .await
        .context("failed to write beta->host direct chat command")?;
    beta_stdin
        .write_all(b"\n")
        .await
        .context("failed to terminate beta machine command line")?;
    drop(beta_stdin);
    drop(host.stdin.take());

    let beta_output = tokio::time::timeout(PROCESS_TIMEOUT, beta.wait_with_output())
        .await
        .context("beta process watchdog timed out")?
        .context("beta process failed while waiting for output")?;
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
        beta_output.status.success(),
        "beta failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&beta_output.stdout),
        String::from_utf8_lossy(&beta_output.stderr)
    );

    let host_events = parse_json_lines(&host_output.stdout)?;
    assert!(host_events.iter().any(|event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("beta".into()))
            && event.get("text") == Some(&Value::String("hi host".into()))
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
