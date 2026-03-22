use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
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
    host.expect_stderr_contains(
        "\"event\":\"room_link_connected\",\"member_id\":\"m2\",\"member_name\":\"beta\"",
    )
    .await?;
    beta.expect_stderr_contains(
        "\"event\":\"room_link_connected\",\"member_id\":\"m1\",\"member_name\":\"host\"",
    )
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
async fn machine_file_channel_requires_transfer_metadata_and_rejects_inline_data() -> Result<()> {
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

    host.expect_stderr_contains(
        "\"event\":\"room_link_connected\",\"member_id\":\"m2\",\"member_name\":\"beta\"",
    )
    .await?;
    beta.expect_stderr_contains(
        "\"event\":\"room_link_connected\",\"member_id\":\"m1\",\"member_name\":\"host\"",
    )
    .await?;

    host.send(
        r#"{"type":"open_channel","channel_id":"c1","kind":"file","to":{"type":"member","member_id":"m2"},"name":"report.pdf","size":1234,"mime":"application/pdf","transfer":{"item_kind":"file","integrity":{"algorithm":"blake3","value":"feedbeef"}}}"#,
    )
    .await?;
    beta.expect_event("file channel_opened", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("c1".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("file".into()))
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
async fn machine_send_file_downloads_and_saves_on_accept() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let source_path = host_home.join("report.txt");
    let saved_path = beta_home.join("report.txt");
    std::fs::write(&source_path, b"mesh file payload")?;

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    host.send(r#"/send --channel f1 --to m2 --path "~/report.txt""#)
        .await?;
    host.expect_event("sender progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.get("name") == Some(&Value::String("report.txt".into()))
            && event.get("phase") == Some(&Value::String("preparing".into()))
    })
    .await?;
    beta.expect_event("file opened from host", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.get("name") == Some(&Value::String("report.txt".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("file".into()))
            && event.pointer("/transfer/integrity") == Some(&Value::Null)
    })
    .await?;
    beta.send("/channel accept f1").await?;
    beta.expect_event("file transfer finalized", |event| {
        event.get("type") == Some(&Value::String("channel_transfer_finalized".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("file".into()))
            && event.pointer("/transfer/integrity/algorithm")
                == Some(&Value::String("blake3".into()))
    })
    .await?;
    beta.expect_event("download progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.get("name") == Some(&Value::String("report.txt".into()))
            && event.get("phase") == Some(&Value::String("downloading".into()))
    })
    .await?;
    beta.expect_event("file received", |event| {
        event.get("type") == Some(&Value::String("channel_path_received".into()))
            && event.get("channel_id") == Some(&Value::String("f1".into()))
            && event.get("path") == Some(&Value::String(saved_path.display().to_string()))
    })
    .await?;

    assert_eq!(std::fs::read(&saved_path)?, b"mesh file payload");

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_send_file_accepts_directory_paths_as_native_folder_transfers() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let source_dir = host_home.join("artpack");
    let nested_dir = source_dir.join("nested");
    let saved_dir = beta_home.join("artpack");
    std::fs::create_dir_all(&nested_dir)?;
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

    host.send(r#"/send --channel folder1 --to m2 --path "~/artpack""#)
        .await?;
    host.expect_event("folder prepare progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.get("name") == Some(&Value::String("artpack".into()))
            && event.get("item_kind") == Some(&Value::String("folder".into()))
            && event.get("phase") == Some(&Value::String("preparing".into()))
    })
    .await?;
    beta.expect_event("folder opened from host", |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.get("name") == Some(&Value::String("artpack".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("folder".into()))
            && event.pointer("/transfer/integrity") == Some(&Value::Null)
    })
    .await?;
    beta.expect_event("folder transfer ready", |event| {
        event.get("type") == Some(&Value::String("channel_transfer_ready".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("folder".into()))
            && event.pointer("/transfer/integrity") == Some(&Value::Null)
    })
    .await?;

    beta.send("/channel accept folder1").await?;
    beta.expect_event("folder download progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.get("name") == Some(&Value::String("artpack".into()))
            && event.get("item_kind") == Some(&Value::String("folder".into()))
            && event.get("phase") == Some(&Value::String("downloading".into()))
    })
    .await?;
    beta.expect_event("folder save progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.get("name") == Some(&Value::String("artpack".into()))
            && event.get("item_kind") == Some(&Value::String("folder".into()))
            && event.get("phase") == Some(&Value::String("exporting".into()))
    })
    .await?;
    beta.expect_event("folder received", |event| {
        event.get("type") == Some(&Value::String("channel_path_received".into()))
            && event.get("channel_id") == Some(&Value::String("folder1".into()))
            && event.get("path") == Some(&Value::String(saved_dir.display().to_string()))
    })
    .await?;

    assert_eq!(std::fs::read(saved_dir.join("a.txt"))?, b"alpha");
    assert_eq!(
        std::fs::read(saved_dir.join("nested").join("b.txt"))?,
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
    let saved_path = beta_home.join("report.txt");
    std::fs::create_dir_all(
        source_path
            .parent()
            .context("source path should have a parent")?,
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
        r#"/send --channel f-all --to all --path "{}""#,
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
    beta.expect_event("broadcast file finalized", |event| {
        event.get("type") == Some(&Value::String("channel_transfer_finalized".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
            && event.pointer("/transfer/integrity/algorithm")
                == Some(&Value::String("blake3".into()))
    })
    .await?;
    gamma
        .expect_event("broadcast file finalized", |event| {
            event.get("type") == Some(&Value::String("channel_transfer_finalized".into()))
                && event.get("channel_id") == Some(&Value::String("f-all".into()))
                && event.pointer("/transfer/integrity/algorithm")
                    == Some(&Value::String("blake3".into()))
        })
        .await?;

    beta.send("/channel accept f-all").await?;
    gamma.send("/channel reject f-all busy").await?;

    beta.expect_event("broadcast download progress", |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
            && event.get("phase") == Some(&Value::String("downloading".into()))
    })
    .await?;
    host.expect_event("gamma rejected", |event| {
        event.get("type") == Some(&Value::String("channel_rejected".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
            && event.get("member_name") == Some(&Value::String("gamma".into()))
    })
    .await?;

    beta.expect_event("broadcast file received", |event| {
        event.get("type") == Some(&Value::String("channel_path_received".into()))
            && event.get("channel_id") == Some(&Value::String("f-all".into()))
            && event.get("path") == Some(&Value::String(saved_path.display().to_string()))
    })
    .await?;
    assert_eq!(std::fs::read(&saved_path)?, b"fanout payload");

    let gamma_events = gamma.events().await;
    assert!(
        !gamma_events.iter().any(|event| {
            event.get("type") == Some(&Value::String("channel_path_received".into()))
                && event.get("channel_id") == Some(&Value::String("f-all".into()))
        }),
        "gamma unexpectedly received rejected file:\n{}",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual performance baseline; run with --ignored --nocapture and optionally SKYFFLA_PERF_FILE_MIB=2048"]
async fn machine_native_file_transfer_reports_baseline() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        println!(
            "skipping perf baseline test: local rendezvous/transport test harness unavailable"
        );
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-perf-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-perf-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let size_mib = perf_file_size_mib();
    let source_path = ensure_perf_source_file(size_mib)?;
    let source_name = source_path
        .file_name()
        .and_then(|value| value.to_str())
        .context("perf source file should have a valid UTF-8 filename")?
        .to_string();
    let source_size = std::fs::metadata(&source_path)?.len();
    let saved_path = beta_home.join(&source_name);

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    let command = serde_json::json!({
        "type": "send_path",
        "channel_id": "perf1",
        "to": { "type": "member", "member_id": "m2" },
        "path": source_path.display().to_string(),
    });

    let send_started_at = Instant::now();
    host.send(&command.to_string()).await?;

    host.expect_event_with_timeout("sender prepare progress", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("perf1".into()))
            && event.get("phase") == Some(&Value::String("preparing".into()))
    })
    .await?;
    beta.expect_event_with_timeout("file opened from host", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("perf1".into()))
            && event.get("name") == Some(&Value::String(source_name.clone()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("file".into()))
    })
    .await?;

    beta.send("/channel accept perf1").await?;

    beta.expect_event_with_timeout("file transfer finalized", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_transfer_finalized".into()))
            && event.get("channel_id") == Some(&Value::String("perf1".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("file".into()))
            && event.pointer("/transfer/integrity/algorithm")
                == Some(&Value::String("blake3".into()))
    })
    .await?;

    beta.expect_event_with_timeout("download progress", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("perf1".into()))
            && event.get("phase") == Some(&Value::String("downloading".into()))
    })
    .await?;

    beta.expect_event_with_timeout("file received", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_path_received".into()))
            && event.get("channel_id") == Some(&Value::String("perf1".into()))
            && event.get("path") == Some(&Value::String(saved_path.display().to_string()))
    })
    .await?;
    let finalized_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("channel_transfer_finalized".into()))
                && event.get("channel_id") == Some(&Value::String("perf1".into()))
        })
        .await
        .context("finalized event timestamp missing")?;
    let first_download_progress_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("transfer_progress".into()))
                && event.get("channel_id") == Some(&Value::String("perf1".into()))
                && event.get("phase") == Some(&Value::String("downloading".into()))
        })
        .await
        .context("download progress timestamp missing")?;
    let received_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("channel_path_received".into()))
                && event.get("channel_id") == Some(&Value::String("perf1".into()))
                && event.get("path") == Some(&Value::String(saved_path.display().to_string()))
        })
        .await
        .context("received event timestamp missing")?;

    assert_eq!(std::fs::metadata(&saved_path)?.len(), source_size);

    let total_elapsed = received_at.duration_since(send_started_at);
    let prepare_elapsed = finalized_at.duration_since(send_started_at);
    let transfer_elapsed = received_at.duration_since(first_download_progress_at);
    let accept_to_receive_elapsed = received_at.duration_since(finalized_at);

    println!(
        "skyffla perf baseline: source={} size={}MiB prep={} transfer={} accept_to_receive={} total={} transfer_rate={} end_to_end_rate={}",
        source_path.display(),
        bytes_to_mib(source_size),
        format_duration_short(prepare_elapsed),
        format_duration_short(transfer_elapsed),
        format_duration_short(accept_to_receive_elapsed),
        format_duration_short(total_elapsed),
        format_mib_per_sec(source_size, transfer_elapsed),
        format_mib_per_sec(source_size, total_elapsed),
    );

    host.shutdown().await?;
    beta.shutdown().await?;
    server.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual folder performance baseline; run with --ignored --nocapture and optionally SKYFFLA_PERF_TREE_MIB=2048 SKYFFLA_PERF_TREE_FILES=128"]
async fn machine_native_folder_transfer_reports_baseline() -> Result<()> {
    let Some(server) = TestServer::spawn().await? else {
        println!(
            "skipping folder perf baseline test: local rendezvous/transport test harness unavailable"
        );
        return Ok(());
    };

    let host_home = fresh_test_dir("skyffla-cli-machine-perf-tree-host");
    let beta_home = fresh_test_dir("skyffla-cli-machine-perf-tree-beta");
    for home in [&host_home, &beta_home] {
        std::fs::create_dir_all(home)
            .with_context(|| format!("failed to create {}", home.display()))?;
    }

    let total_mib = perf_tree_size_mib();
    let file_count = perf_tree_file_count();
    let source_dir = ensure_perf_source_tree(total_mib, file_count)?;
    let source_name = source_dir
        .file_name()
        .and_then(|value| value.to_str())
        .context("perf source tree should have a valid UTF-8 directory name")?
        .to_string();
    let (source_size, actual_file_count) = directory_size_and_file_count(&source_dir)?;
    let saved_dir = beta_home.join(&source_name);

    let room = unique_room_name();
    let mut host = MachineProc::spawn("host", &room, &server.url, "host", &host_home).await?;
    wait_for_room_ready(&server.url, &room).await?;
    let mut beta = MachineProc::spawn("join", &room, &server.url, "beta", &beta_home).await?;

    host.expect_stderr_contains("\"member_name\":\"beta\"")
        .await?;
    beta.expect_stderr_contains("\"member_name\":\"host\"")
        .await?;

    let command = serde_json::json!({
        "type": "send_path",
        "channel_id": "perftree1",
        "to": { "type": "member", "member_id": "m2" },
        "path": source_dir.display().to_string(),
    });

    let send_started_at = Instant::now();
    host.send(&command.to_string()).await?;

    host.expect_event_with_timeout("sender folder prepare progress", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("perftree1".into()))
            && event.get("phase") == Some(&Value::String("preparing".into()))
            && event.get("item_kind") == Some(&Value::String("folder".into()))
    })
    .await?;
    beta.expect_event_with_timeout("folder opened from host", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_opened".into()))
            && event.get("channel_id") == Some(&Value::String("perftree1".into()))
            && event.get("name") == Some(&Value::String(source_name.clone()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("folder".into()))
            && event.pointer("/transfer/integrity") == Some(&Value::Null)
    })
    .await?;

    beta.send("/channel accept perftree1").await?;

    beta.expect_event_with_timeout("folder transfer ready", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_transfer_ready".into()))
            && event.get("channel_id") == Some(&Value::String("perftree1".into()))
            && event.pointer("/transfer/item_kind") == Some(&Value::String("folder".into()))
            && event.pointer("/transfer/integrity") == Some(&Value::Null)
    })
    .await?;

    beta.expect_event_with_timeout("folder download progress", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("transfer_progress".into()))
            && event.get("channel_id") == Some(&Value::String("perftree1".into()))
            && event.get("phase") == Some(&Value::String("downloading".into()))
            && event.get("item_kind") == Some(&Value::String("folder".into()))
    })
    .await?;

    beta.expect_event_with_timeout("folder received", perf_timeout(), |event| {
        event.get("type") == Some(&Value::String("channel_path_received".into()))
            && event.get("channel_id") == Some(&Value::String("perftree1".into()))
            && event.get("path") == Some(&Value::String(saved_dir.display().to_string()))
    })
    .await?;

    let ready_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("channel_transfer_ready".into()))
                && event.get("channel_id") == Some(&Value::String("perftree1".into()))
        })
        .await
        .context("folder ready event timestamp missing")?;
    let first_download_progress_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("transfer_progress".into()))
                && event.get("channel_id") == Some(&Value::String("perftree1".into()))
                && event.get("phase") == Some(&Value::String("downloading".into()))
        })
        .await
        .context("folder download progress timestamp missing")?;
    let received_at = beta
        .event_seen_at(|event| {
            event.get("type") == Some(&Value::String("channel_path_received".into()))
                && event.get("channel_id") == Some(&Value::String("perftree1".into()))
                && event.get("path") == Some(&Value::String(saved_dir.display().to_string()))
        })
        .await
        .context("folder received event timestamp missing")?;

    let (saved_size, saved_file_count) = directory_size_and_file_count(&saved_dir)?;
    assert_eq!(saved_size, source_size);
    assert_eq!(saved_file_count, actual_file_count);

    let total_elapsed = received_at.duration_since(send_started_at);
    let prepare_elapsed = ready_at.duration_since(send_started_at);
    let transfer_elapsed = received_at.duration_since(first_download_progress_at);
    let accept_to_receive_elapsed = received_at.duration_since(ready_at);

    println!(
        "skyffla folder perf baseline: source={} files={} size={}MiB prep={} transfer={} accept_to_receive={} total={} transfer_rate={} end_to_end_rate={}",
        source_dir.display(),
        actual_file_count,
        bytes_to_mib(source_size),
        format_duration_short(prepare_elapsed),
        format_duration_short(transfer_elapsed),
        format_duration_short(accept_to_receive_elapsed),
        format_duration_short(total_elapsed),
        format_mib_per_sec(source_size, transfer_elapsed),
        format_mib_per_sec(source_size, total_elapsed),
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
    stdout_seen: Arc<Mutex<Vec<SeenEvent>>>,
    stderr_seen: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct SeenEvent {
    seen_at: Instant,
    value: Value,
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
            .arg("--download-dir")
            .arg(home)
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
        self.expect_event_with_timeout(label, PROCESS_TIMEOUT, predicate)
            .await
    }

    async fn expect_event_with_timeout<F>(
        &mut self,
        label: &str,
        timeout_window: Duration,
        predicate: F,
    ) -> Result<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let deadline = Instant::now() + timeout_window;
        loop {
            if let Some(found) = self
                .stdout_seen
                .lock()
                .await
                .iter()
                .find(|event| predicate(&event.value))
                .map(|event| event.value.clone())
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
        self.stdout_seen
            .lock()
            .await
            .iter()
            .map(|event| event.value.clone())
            .collect()
    }

    async fn event_seen_at<F>(&self, predicate: F) -> Option<Instant>
    where
        F: Fn(&Value) -> bool,
    {
        self.stdout_seen
            .lock()
            .await
            .iter()
            .find(|event| predicate(&event.value))
            .map(|event| event.seen_at)
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
            .map(|event| event.value.to_string())
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
    seen: Arc<Mutex<Vec<SeenEvent>>>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                seen.lock().await.push(SeenEvent {
                    seen_at: Instant::now(),
                    value: value.clone(),
                });
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

fn perf_file_size_mib() -> u64 {
    std::env::var("SKYFFLA_PERF_FILE_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(512)
}

fn perf_tree_size_mib() -> u64 {
    std::env::var("SKYFFLA_PERF_TREE_MIB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(512)
}

fn perf_tree_file_count() -> usize {
    std::env::var("SKYFFLA_PERF_TREE_FILES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(128)
}

fn perf_timeout() -> Duration {
    std::env::var("SKYFFLA_PERF_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(180))
}

fn ensure_perf_source_file(size_mib: u64) -> Result<PathBuf> {
    let cache_root = std::env::temp_dir().join("skyffla-transfer-perf");
    std::fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;
    let path = cache_root.join(format!("perf-random-{size_mib}m.bin"));
    let expected_len = size_mib * 1024 * 1024;
    if std::fs::metadata(&path)
        .map(|meta| meta.len() == expected_len)
        .unwrap_or(false)
    {
        return Ok(path);
    }

    let file = File::create(&path)
        .with_context(|| format!("failed to create perf source file {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut buffer = vec![0u8; 8 * 1024 * 1024];
    let mut remaining = expected_len;
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len() as u64) as usize;
        fill_pseudorandom_bytes(&mut buffer[..chunk_len], &mut state);
        writer
            .write_all(&buffer[..chunk_len])
            .with_context(|| format!("failed writing perf source file {}", path.display()))?;
        remaining -= chunk_len as u64;
    }
    writer
        .flush()
        .with_context(|| format!("failed flushing perf source file {}", path.display()))?;
    Ok(path)
}

fn ensure_perf_source_tree(total_mib: u64, file_count: usize) -> Result<PathBuf> {
    let cache_root = std::env::temp_dir().join("skyffla-transfer-perf");
    std::fs::create_dir_all(&cache_root)
        .with_context(|| format!("failed to create {}", cache_root.display()))?;
    let root = cache_root.join(format!("perf-tree-{total_mib}m-{file_count}f"));
    let expected_len = total_mib * 1024 * 1024;
    if let Ok((actual_len, actual_files)) = directory_size_and_file_count(&root) {
        if actual_len == expected_len && actual_files == file_count {
            return Ok(root);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create perf source tree {}", root.display()))?;
    let mut state = 0xD1B5_4A32_C19F_8E77u64;
    let mut remaining = expected_len;
    for index in 0..file_count {
        let files_left = (file_count - index) as u64;
        let target_len = if files_left == 1 {
            remaining
        } else {
            remaining / files_left
        };
        let dir = root
            .join(format!("branch-{:02}", index % 8))
            .join(format!("set-{:02}", (index / 8) % 8));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join(format!("part-{index:04}.bin"));
        write_pseudorandom_file(&path, target_len, &mut state)?;
        remaining = remaining.saturating_sub(target_len);
    }
    Ok(root)
}

fn write_pseudorandom_file(path: &Path, len: u64, state: &mut u64) -> Result<()> {
    let file = File::create(path)
        .with_context(|| format!("failed to create perf file {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut buffer = vec![0u8; 8 * 1024 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len() as u64) as usize;
        fill_pseudorandom_bytes(&mut buffer[..chunk_len], state);
        writer
            .write_all(&buffer[..chunk_len])
            .with_context(|| format!("failed writing perf file {}", path.display()))?;
        remaining -= chunk_len as u64;
    }
    writer
        .flush()
        .with_context(|| format!("failed flushing perf file {}", path.display()))?;
    Ok(())
}

fn directory_size_and_file_count(root: &Path) -> Result<(u64, usize)> {
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let mut total_size = 0_u64;
    let mut total_files = 0_usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .with_context(|| format!("failed to read {}", current.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to iterate {}", current.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to stat {}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                total_files += 1;
                total_size += entry
                    .metadata()
                    .with_context(|| format!("failed to read metadata for {}", path.display()))?
                    .len();
            }
        }
    }
    Ok((total_size, total_files))
}

fn fill_pseudorandom_bytes(buf: &mut [u8], state: &mut u64) {
    for chunk in buf.chunks_mut(8) {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        let len = chunk.len();
        chunk.copy_from_slice(&value[..len]);
    }
}

fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn format_mib_per_sec(bytes: u64, elapsed: Duration) -> String {
    if elapsed.is_zero() {
        return "inf MiB/s".into();
    }
    let mib = bytes as f64 / (1024.0 * 1024.0);
    format!("{:.1}MiB/s", mib / elapsed.as_secs_f64())
}

fn format_duration_short(elapsed: Duration) -> String {
    if elapsed.as_secs_f64() >= 1.0 {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        format!("{:.0}ms", elapsed.as_secs_f64() * 1000.0)
    }
}
