use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::sleep;

mod support;

use support::{
    acquire_local_discovery_test_guard, assert_local_mode_stderr, fresh_test_dir,
    local_discovery_available, unique_room_name, LOCAL_DISCOVERY_BOOTSTRAP_DELAY,
    LOCAL_JOIN_PROMOTION_DELAY, PROCESS_TIMEOUT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_host_to_join_transfers_payload_without_rendezvous() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-host");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let payload = b"hello from local discovery\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut host = Command::new(bin);
    host.arg("host")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut host = host.spawn().context("failed to spawn local host process")?;

    let mut host_stdin = host.stdin.take().context("host stdin missing")?;
    host_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into local host stdin")?;
    drop(host_stdin);

    sleep(LOCAL_DISCOVERY_BOOTSTRAP_DELAY).await;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn local join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("local join process watchdog timed out")?
        .context("local join process failed while waiting for output")?;
    if !join_output.status.success() {
        let _ = host.kill().await;
        let host_output = host
            .wait_with_output()
            .await
            .context("local host process failed while collecting failure output")?;
        anyhow::bail!(
            "local join failed:\nstdout={}\nstderr={}\nhost stdout={}\nhost stderr={}",
            String::from_utf8_lossy(&join_output.stdout),
            String::from_utf8_lossy(&join_output.stderr),
            String::from_utf8_lossy(&host_output.stdout),
            String::from_utf8_lossy(&host_output.stderr)
        );
    }
    let host_output = tokio::time::timeout(PROCESS_TIMEOUT, host.wait_with_output())
        .await
        .context("local host process watchdog timed out")?
        .context("local host process failed while waiting for output")?;

    assert!(
        host_output.status.success(),
        "local host failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&host_output.stdout),
        String::from_utf8_lossy(&host_output.stderr)
    );
    assert_eq!(join_output.stdout, payload);
    assert_local_mode_stderr(&join_output.stderr);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_join_to_join_promotes_first_peer_without_rendezvous() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-join");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let payload = b"hello from local join discovery\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut first_join = Command::new(bin);
    first_join
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
    let mut first_join = first_join
        .spawn()
        .context("failed to spawn first local join process")?;

    let mut first_stdin = first_join
        .stdin
        .take()
        .context("first join stdin missing")?;
    first_stdin
        .write_all(payload)
        .await
        .context("failed to write stdio payload into first local join stdin")?;
    drop(first_stdin);

    sleep(LOCAL_JOIN_PROMOTION_DELAY).await;

    let mut second_join = Command::new(bin);
    second_join
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let second_join = second_join
        .spawn()
        .context("failed to spawn second local join process")?;

    let second_output = tokio::time::timeout(PROCESS_TIMEOUT, second_join.wait_with_output())
        .await
        .context("second local join process watchdog timed out")?
        .context("second local join process failed while waiting for output")?;
    if !second_output.status.success() {
        let _ = first_join.kill().await;
        let first_output = first_join
            .wait_with_output()
            .await
            .context("first local join process failed while collecting failure output")?;
        anyhow::bail!(
            "second local join failed:\nstdout={}\nstderr={}\nfirst stdout={}\nfirst stderr={}",
            String::from_utf8_lossy(&second_output.stdout),
            String::from_utf8_lossy(&second_output.stderr),
            String::from_utf8_lossy(&first_output.stdout),
            String::from_utf8_lossy(&first_output.stderr)
        );
    }
    let first_output = tokio::time::timeout(PROCESS_TIMEOUT, first_join.wait_with_output())
        .await
        .context("first local join process watchdog timed out")?
        .context("first local join process failed while waiting for output")?;

    assert!(
        first_output.status.success(),
        "first local join failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&first_output.stdout),
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert_eq!(second_output.stdout, payload);
    assert_local_mode_stderr(&second_output.stderr);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_local_multiple_hosts_same_stream_one_join_connects_to_one_host() -> Result<()> {
    let _guard = acquire_local_discovery_test_guard()?;
    if !local_discovery_available().await? {
        return Ok(());
    }

    let home_dir = fresh_test_dir("skyffla-cli-stdio-local-multi-host");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let first_payload = b"hello from first host\n";
    let second_payload = b"hello from second host\n";
    let bin = env!("CARGO_BIN_EXE_skyffla");
    let unreachable_server = "http://127.0.0.1:9";

    let mut first_host = Command::new(bin);
    first_host
        .arg("host")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("first-host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut first_host = first_host
        .spawn()
        .context("failed to spawn first local host process")?;

    let mut second_host = Command::new(bin);
    second_host
        .arg("host")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("second-host")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second_host = second_host
        .spawn()
        .context("failed to spawn second local host process")?;

    let mut first_stdin = first_host
        .stdin
        .take()
        .context("first local host stdin missing")?;
    first_stdin
        .write_all(first_payload)
        .await
        .context("failed to write payload into first local host stdin")?;
    drop(first_stdin);

    let mut second_stdin = second_host
        .stdin
        .take()
        .context("second local host stdin missing")?;
    second_stdin
        .write_all(second_payload)
        .await
        .context("failed to write payload into second local host stdin")?;
    drop(second_stdin);

    sleep(LOCAL_DISCOVERY_BOOTSTRAP_DELAY).await;

    let mut join = Command::new(bin);
    join.arg("join")
        .arg(&room)
        .arg("--local")
        .arg("--server")
        .arg(unreachable_server)
        .arg("--name")
        .arg("join")
        .arg("--stdio")
        .arg("--json")
        .env("HOME", &home_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let join = join.spawn().context("failed to spawn local join process")?;

    let join_output = tokio::time::timeout(PROCESS_TIMEOUT, join.wait_with_output())
        .await
        .context("multi-host local join watchdog timed out")?
        .context("multi-host local join failed while waiting for output")?;
    assert!(
        join_output.status.success(),
        "multi-host local join failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&join_output.stdout),
        String::from_utf8_lossy(&join_output.stderr)
    );

    let connected_to_first = join_output.stdout == first_payload;
    let connected_to_second = join_output.stdout == second_payload;
    assert!(
        connected_to_first || connected_to_second,
        "join should receive one host payload, got:\n{}",
        String::from_utf8_lossy(&join_output.stdout)
    );
    assert_ne!(connected_to_first, connected_to_second);
    assert_local_mode_stderr(&join_output.stderr);

    let first_result = collect_host_result(first_host, "first local host").await?;
    let second_result = collect_host_result(second_host, "second local host").await?;

    match (connected_to_first, connected_to_second) {
        (true, false) => {
            assert_host_completed(&first_result, "first local host");
            assert_host_terminated_while_waiting(&second_result, "second local host");
        }
        (false, true) => {
            assert_host_terminated_while_waiting(&first_result, "first local host");
            assert_host_completed(&second_result, "second local host");
        }
        _ => unreachable!(),
    }

    Ok(())
}

enum HostResult {
    Completed(std::process::ExitStatus),
    Terminated(std::process::ExitStatus),
}

async fn collect_host_result(mut host: tokio::process::Child, label: &str) -> Result<HostResult> {
    match tokio::time::timeout(Duration::from_secs(3), host.wait()).await {
        Ok(result) => {
            let status = result.with_context(|| format!("{label} failed while waiting"))?;
            Ok(HostResult::Completed(status))
        }
        Err(_) => {
            let _ = host.kill().await;
            let status = host
                .wait()
                .await
                .with_context(|| format!("{label} failed while collecting terminated status"))?;
            Ok(HostResult::Terminated(status))
        }
    }
}

fn assert_host_completed(result: &HostResult, label: &str) {
    match result {
        HostResult::Completed(status) => {
            assert!(
                status.success(),
                "{label} should have completed successfully, got status {status}"
            );
        }
        HostResult::Terminated(status) => {
            panic!(
                "{label} should have completed, but it was still waiting and had status {status}"
            );
        }
    }
}

fn assert_host_terminated_while_waiting(result: &HostResult, label: &str) {
    match result {
        HostResult::Completed(status) => {
            panic!("{label} should have remained idle, but it exited with status {status}");
        }
        HostResult::Terminated(status) => {
            assert!(
                !status.success(),
                "{label} should have been terminated while waiting, but exited successfully"
            );
        }
    }
}
