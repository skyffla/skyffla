use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

mod support;

use support::{
    assert_local_mode_stderr, fresh_test_dir, local_discovery_available, unique_room_name,
    PROCESS_TIMEOUT,
};

const SIMULTANEOUS_JOIN_RACE_ITERATIONS: usize = 5;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_simultaneous_join_elects_single_host_without_rendezvous() -> Result<()> {
    if !local_discovery_available().await? {
        return Ok(());
    }

    for iteration in 0..SIMULTANEOUS_JOIN_RACE_ITERATIONS {
        run_single_simultaneous_join_race(iteration).await?;
    }

    Ok(())
}

async fn run_single_simultaneous_join_race(iteration: usize) -> Result<()> {
    let home_dir = fresh_test_dir(&format!("skyffla-cli-stdio-local-race-{iteration}"));
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let payload = b"hello from simultaneous local join\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut first = Command::new(bin);
    first
        .arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("first")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut first = first.spawn().with_context(|| {
        format!("failed to spawn first simultaneous local join process for iteration {iteration}")
    })?;

    let mut second = Command::new(bin);
    second
        .arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("second")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second = second.spawn().with_context(|| {
        format!("failed to spawn second simultaneous local join process for iteration {iteration}")
    })?;

    let mut first_stdin = first
        .stdin
        .take()
        .context("first simultaneous join stdin missing")?;
    first_stdin
        .write_all(payload)
        .await
        .context("failed to write payload into first simultaneous join stdin")?;
    drop(first_stdin);

    let mut second_stdin = second
        .stdin
        .take()
        .context("second simultaneous join stdin missing")?;
    second_stdin
        .write_all(payload)
        .await
        .context("failed to write payload into second simultaneous join stdin")?;
    drop(second_stdin);

    let first_output = tokio::time::timeout(PROCESS_TIMEOUT, first.wait_with_output())
        .await
        .with_context(|| {
            format!("first simultaneous local join watchdog timed out in iteration {iteration}")
        })?
        .context("first simultaneous local join failed while waiting for output")?;
    let second_output = tokio::time::timeout(PROCESS_TIMEOUT, second.wait_with_output())
        .await
        .with_context(|| {
            format!("second simultaneous local join watchdog timed out in iteration {iteration}")
        })?
        .context("second simultaneous local join failed while waiting for output")?;

    assert!(
        first_output.status.success(),
        "first simultaneous local join failed in iteration {iteration}:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first_output.stdout),
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert!(
        second_output.status.success(),
        "second simultaneous local join failed in iteration {iteration}:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&second_output.stdout),
        String::from_utf8_lossy(&second_output.stderr)
    );

    let first_received = first_output.stdout == payload;
    let second_received = second_output.stdout == payload;
    assert_ne!(
        first_received, second_received,
        "expected exactly one simultaneous joiner to receive the stdio payload in iteration {iteration}:\nfirst stdout={}\nsecond stdout={}",
        String::from_utf8_lossy(&first_output.stdout),
        String::from_utf8_lossy(&second_output.stdout)
    );

    if !first_received {
        assert!(
            first_output.stdout.is_empty(),
            "host-side simultaneous join stdout should be empty in iteration {iteration}:\n{}",
            String::from_utf8_lossy(&first_output.stdout)
        );
    }
    if !second_received {
        assert!(
            second_output.stdout.is_empty(),
            "host-side simultaneous join stdout should be empty in iteration {iteration}:\n{}",
            String::from_utf8_lossy(&second_output.stdout)
        );
    }

    assert_local_mode_stderr(&first_output.stderr);
    assert_local_mode_stderr(&second_output.stderr);

    Ok(())
}
