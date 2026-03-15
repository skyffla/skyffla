use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};

mod support;

use support::{fresh_test_dir, unique_room_name, wait_for_room_ready, TestServer, PROCESS_TIMEOUT};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_host_to_join_delivers_room_events_and_direct_chat() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let home_dir = fresh_test_dir("skyffla-cli-machine");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &home_dir).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut join = MachineProc::spawn("join", &room, &server.url, "join", &home_dir).await?;

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

    host.send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello machine room"}"#)
        .await?;
    join.expect_event("host chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("host".into()))
            && event.get("text") == Some(&Value::String("hello machine room".into()))
    })
    .await?;

    host.shutdown().await?;
    join.shutdown().await?;
    server.abort();
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(r#"{"type":"send_chat","to":{"type":"all"},"text":"hello everyone"}"#)
        .await?;
    beta.expect_event("broadcast chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("host".into()))
            && event.get("text") == Some(&Value::String("hello everyone".into()))
    })
    .await?;
    gamma
        .expect_event("broadcast chat", |event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("from_name") == Some(&Value::String("host".into()))
                && event.get("text") == Some(&Value::String("hello everyone".into()))
        })
        .await?;

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    let beta_stderr = beta.stderr_lines().await;
    let beta_events = beta.events().await;
    let gamma_id = linked_member_id_named(&beta_stderr, "gamma")
        .or_else(|| member_id_named(&beta_events, "gamma"))
        .context("beta did not learn gamma member id")?;

    beta.send(
        &format!(
            r#"{{"type":"send_chat","to":{{"type":"member","member_id":"{gamma_id}"}},"text":"secret hello"}}"#
        ),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let beta_events = beta.events().await;
    assert!(
        !beta_events
            .iter()
            .any(|event| event.get("type") == Some(&Value::String("error".into()))),
        "beta emitted an unexpected error after direct chat:\n{}",
        beta.debug_dump().await
    );
    gamma
        .expect_event("direct beta->gamma chat", |event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("from_name") == Some(&Value::String("beta".into()))
                && event.get("text") == Some(&Value::String("secret hello".into()))
        })
        .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let host_events = host.events().await;
    assert!(
        !host_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("chat".into()))
                && event.get("text") == Some(&Value::String("secret hello".into()))
        }),
        "host unexpectedly saw direct beta->gamma chat:\n{}",
        host.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_event("beta joined", |event| {
        event.get("type") == Some(&Value::String("member_joined".into()))
            && event.pointer("/member/name") == Some(&Value::String("beta".into()))
    })
    .await?;
    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    beta.send(r#"{"type":"send_chat","to":{"type":"member","member_id":"m1"},"text":"hi host"}"#)
        .await?;
    host.expect_event("beta->host chat", |event| {
        event.get("type") == Some(&Value::String("chat".into()))
            && event.get("from_name") == Some(&Value::String("beta".into()))
            && event.get("text") == Some(&Value::String("hi host".into()))
    })
    .await?;

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_joiner_channel_data_flows_directly_to_another_joiner() -> Result<()> {
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    let beta_stderr = beta.stderr_lines().await;
    let beta_events = beta.events().await;
    let gamma_id = linked_member_id_named(&beta_stderr, "gamma")
        .or_else(|| member_id_named(&beta_events, "gamma"))
        .context("beta did not learn gamma member id")?;

    beta.send(
        &format!(
            r#"{{"type":"open_channel","channel_id":"c1","kind":"machine","to":{{"type":"member","member_id":"{gamma_id}"}}}}"#
        ),
    )
    .await?;
    gamma
        .expect_event("channel_opened", |event| {
            event.get("type") == Some(&Value::String("channel_opened".into()))
                && event.get("channel_id") == Some(&Value::String("c1".into()))
        })
        .await?;

    gamma
        .send(r#"{"type":"send_channel_data","channel_id":"c1","body":"sketch line"}"#)
        .await?;
    beta.expect_event("channel_data", |event| {
        event.get("type") == Some(&Value::String("channel_data".into()))
            && event.get("from_name") == Some(&Value::String("gamma".into()))
            && event.get("body") == Some(&Value::String("sketch line".into()))
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let host_events = host.events().await;
    assert!(
        !host_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_data".into()))
                && event.get("body") == Some(&Value::String("sketch line".into()))
        }),
        "host unexpectedly saw direct beta<->gamma channel data:\n{}",
        host.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_file_channel_requires_blob_metadata_and_rejects_inline_data() -> Result<()> {
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(
        r#"{"type":"open_channel","channel_id":"c1","kind":"file","to":{"type":"member","member_id":"m2"},"name":"report.pdf","size":1234,"mime":"application/pdf","blob":{"hash":"abc123","format":"blob"}}"#,
    )
    .await?;
    beta.expect_event("file channel_opened", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
            && event.pointer("/blob/hash") == Some(&Value::String("abc123".into()))
    })
    .await?;

    host.send(r#"{"type":"send_channel_data","channel_id":"c1","body":"raw bytes"}"#)
        .await?;
    host.expect_event("file inline-data rejected", |event| {
        event.get("type") == Some(&Value::String("error".into()))
            && event.get("code") == Some(&Value::String("command_failed".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let beta_events = beta.events().await;
    assert!(
        !beta_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_data".into()))
                && event.get("body") == Some(&Value::String("raw bytes".into()))
        }),
        "beta unexpectedly saw inline file channel data:\n{}",
        beta.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_send_file_downloads_and_exports_on_accept() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let source_path = fresh_test_dir("skyffla-cli-machine-file-source").join("report.txt");
    let export_path = fresh_test_dir("skyffla-cli-machine-file-export").join("report.txt");
    std::fs::create_dir_all(
        source_path
            .parent()
            .context("source path should have a parent")?,
    )?;
    std::fs::create_dir_all(
        export_path
            .parent()
            .context("export path should have a parent")?,
    )?;
    std::fs::write(&source_path, b"mesh file payload")?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(&format!(
        r#"/file send --channel f1 --to m2 --path "{}""#,
        source_path.display()
    ))
    .await?;
    beta.expect_event("file opened from host", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.pointer("/blob/hash").is_some()
    })
    .await?;

    beta.send("/channel accept f1").await?;
    beta.expect_event("file ready", |event| {
        event.get("type") == Some(&Value::String("channel_file_ready".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
    })
    .await?;

    beta.send(&format!(
        r#"/file export --channel f1 --path "{}""#,
        export_path.display()
    ))
    .await?;
    beta.expect_event("file exported", |event| {
        event.get("type") == Some(&Value::String("channel_file_exported".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
    })
    .await?;

    assert_eq!(std::fs::read(&export_path)?, b"mesh file payload");

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_send_file_accepts_directory_paths_as_collections() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let source_dir = fresh_test_dir("skyffla-cli-machine-folder-source");
    let nested_dir = source_dir.join("nested");
    let export_dir = fresh_test_dir("skyffla-cli-machine-folder-export");
    std::fs::create_dir_all(&nested_dir)?;
    std::fs::create_dir_all(&export_dir)?;
    std::fs::write(source_dir.join("a.txt"), b"alpha")?;
    std::fs::write(nested_dir.join("b.txt"), b"beta")?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(&format!(
        r#"/file send --channel folder1 --to m2 --path "{}""#,
        source_dir.display()
    ))
    .await?;
    beta.expect_event("folder opened from host", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.pointer("/blob/format") == Some(&Value::String("collection".into()))
    })
    .await?;

    beta.send("/channel accept folder1").await?;
    beta.expect_event("folder ready", |event| {
        event.get("type") == Some(&Value::String("channel_file_ready".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.pointer("/blob/format") == Some(&Value::String("collection".into()))
    })
    .await?;

    beta.send(&format!(
        r#"/file export --channel folder1 --path "{}""#,
        export_dir.display()
    ))
    .await?;
    beta.expect_event("folder exported", |event| {
        event.get("type") == Some(&Value::String("channel_file_exported".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
    })
    .await?;

    assert_eq!(std::fs::read(export_dir.join("a.txt"))?, b"alpha");
    assert_eq!(
        std::fs::read(export_dir.join("nested").join("b.txt"))?,
        b"beta"
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_broadcast_file_accepts_and_rejects_independently() -> Result<()> {
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

    let source_path = fresh_test_dir("skyffla-cli-machine-fanout-source").join("report.txt");
    let export_path = fresh_test_dir("skyffla-cli-machine-fanout-export").join("report.txt");
    std::fs::create_dir_all(
        source_path
            .parent()
            .context("source path should have a parent")?,
    )?;
    std::fs::create_dir_all(
        export_path
            .parent()
            .context("export path should have a parent")?,
    )?;
    std::fs::write(&source_path, b"fanout payload")?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;

    host.send(&format!(
        r#"/file send --channel f-all --to all --path "{}""#,
        source_path.display()
    ))
    .await?;
    beta.expect_event("broadcast file opened", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
    })
    .await?;
    gamma
        .expect_event("broadcast file opened", |event| {
            event.get("type") == Some(&Value::String("channel_opened".into()))
                && event.get("channel_id") == Some(&Value::String("f-all".into()))
        })
        .await?;

    beta.send("/channel accept f-all").await?;
    gamma.send("/channel reject f-all busy").await?;

    beta.expect_event("broadcast file ready", |event| {
        event.get("type") == Some(&Value::String("channel_file_ready".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
    })
    .await?;
    host.expect_event("gamma rejected", |event| {
        event.get("type") == Some(&Value::String("channel_rejected".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
            && event.get("member_name") == Some(&Value::String("gamma".into()))
    })
    .await?;

    beta.send(&format!(
        r#"/file export --channel f-all --path "{}""#,
        export_path.display()
    ))
    .await?;
    beta.expect_event("broadcast file exported", |event| {
        event.get("type") == Some(&Value::String("channel_file_exported".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
    })
    .await?;
    assert_eq!(std::fs::read(&export_path)?, b"fanout payload");

    let gamma_events = gamma.events().await;
    assert!(
        !gamma_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_file_ready".into()))
                && event.get("channel_id") == Some(&Value::String("f-all".into()))
        }),
        "gamma unexpectedly marked rejected file ready:\n{}",
        gamma.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_rejected_channel_emits_error_for_late_sender_data() -> Result<()> {
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    host.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    let beta_stderr = beta.stderr_lines().await;
    let beta_events = beta.events().await;
    let gamma_id = linked_member_id_named(&beta_stderr, "gamma")
        .or_else(|| member_id_named(&beta_events, "gamma"))
        .context("beta did not learn gamma member id")?;

    beta.send(
        &format!(
            r#"{{"type":"open_channel","channel_id":"c1","kind":"machine","to":{{"type":"member","member_id":"{gamma_id}"}}}}"#
        ),
    )
    .await?;
    gamma
        .expect_event("channel_opened", |event| {
            event.get("type") == Some(&Value::String("channel_opened".into()))
                && event.get("channel_id") == Some(&Value::String("c1".into()))
        })
        .await?;

    gamma
        .send(r#"{"type":"reject_channel","channel_id":"c1","reason":"busy"}"#)
        .await?;
    beta.expect_event("channel_rejected", |event| {
        event.get("type") == Some(&Value::String("channel_rejected".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    beta.send(r#"{"type":"send_channel_data","channel_id":"c1","body":"too late"}"#)
        .await?;
    beta.expect_event("late sender error", |event| {
        event.get("type") == Some(&Value::String("error".into()))
            && event.get("code") == Some(&Value::String("command_failed".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let gamma_events = gamma.events().await;
    assert!(
        !gamma_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_data".into()))
                && event.get("body") == Some(&Value::String("too late".into()))
        }),
        "gamma unexpectedly saw late channel data after rejection:\n{}",
        gamma.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_broadcast_channel_data_reaches_host_and_other_joiners() -> Result<()> {
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;
    let mut gamma = MachineProc::spawn("join", &room, &server.url, "gamma", &gamma_home).await?;

    beta.expect_stderr_contains("\"member_name\":\"gamma\"")
        .await?;
    gamma
        .expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;

    beta.send(r#"{"type":"open_channel","channel_id":"c1","kind":"machine","to":{"type":"all"}}"#)
        .await?;
    host.expect_event("broadcast channel_opened", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;
    gamma
        .expect_event("broadcast channel_opened", |event| {
            event.get("type") == Some(&Value::String("channel_opened".into()))
                && event.get("channel_id") == Some(&Value::String("c1".into()))
        })
        .await?;

    gamma
        .send(r#"{"type":"send_channel_data","channel_id":"c1","body":"fanout line"}"#)
        .await?;
    beta.expect_event("broadcast channel_data", |event| {
        event.get("type") == Some(&Value::String("channel_data".into()))
            && event.get("from_name") == Some(&Value::String("gamma".into()))
            && event.get("body") == Some(&Value::String("fanout line".into()))
    })
    .await?;
    host.expect_event("broadcast channel_data", |event| {
        event.get("type") == Some(&Value::String("channel_data".into()))
            && event.get("from_name") == Some(&Value::String("gamma".into()))
            && event.get("body") == Some(&Value::String("fanout line".into()))
    })
    .await?;

    host.shutdown().await?;
    beta.shutdown().await?;
    gamma.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_closed_channel_emits_error_for_late_host_data() -> Result<()> {
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
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(
        r#"{"type":"open_channel","channel_id":"c1","kind":"machine","to":{"type":"member","member_id":"m2"}}"#,
    )
    .await?;
    beta.expect_event("channel_opened", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    beta.send(r#"{"type":"close_channel","channel_id":"c1","reason":"done"}"#)
        .await?;
    host.expect_event("channel_closed", |event| {
        event.get("type") == Some(&Value::String("channel_closed".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    host.send(r#"{"type":"send_channel_data","channel_id":"c1","body":"after close"}"#)
        .await?;
    host.expect_event("late host-data error", |event| {
        event.get("type") == Some(&Value::String("error".into()))
            && event.get("code") == Some(&Value::String("command_failed".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
    })
    .await?;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let beta_events = beta.events().await;
    assert!(
        !beta_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_data".into()))
                && event.get("body") == Some(&Value::String("after close".into()))
        }),
        "beta unexpectedly saw late host channel data after close:\n{}",
        beta.debug_dump().await
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
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
            .arg("machine")
            .arg("--server")
            .arg(server_url)
            .arg("--name")
            .arg(name)
            .arg("--json")
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {role} process"))?;
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

    async fn events(&self) -> Vec<Value> {
        self.stdout_seen.lock().await.clone()
    }

    async fn stderr_lines(&self) -> Vec<String> {
        self.stderr_seen.lock().await.clone()
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

fn member_id_named(events: &[Value], name: &str) -> Option<String> {
    for event in events {
        if let Some(members) = event.get("members").and_then(Value::as_array) {
            if let Some(member_id) = members.iter().find_map(|member| {
                (member.get("name") == Some(&Value::String(name.into())))
                    .then(|| member.get("member_id").and_then(Value::as_str))
                    .flatten()
            }) {
                return Some(member_id.to_string());
            }
        }
        if event.pointer("/member/name") == Some(&Value::String(name.into())) {
            if let Some(member_id) = event.pointer("/member/member_id").and_then(Value::as_str) {
                return Some(member_id.to_string());
            }
        }
    }
    None
}

fn linked_member_id_named(lines: &[String], name: &str) -> Option<String> {
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("event") == Some(&Value::String("room_link_connected".into()))
            && value.get("member_name") == Some(&Value::String(name.into()))
        {
            if let Some(member_id) = value.get("member_id").and_then(Value::as_str) {
                return Some(member_id.to_string());
            }
        }
    }
    None
}
