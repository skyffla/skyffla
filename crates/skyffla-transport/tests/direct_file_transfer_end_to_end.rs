use std::time::{SystemTime, UNIX_EPOCH};

use skyffla_transport::{IrohTransport, TransportError};

async fn bind_or_skip() -> Option<IrohTransport> {
    match IrohTransport::bind().await {
        Ok(transport) => Some(transport),
        Err(error) if is_socket_permission_error(&error) => None,
        Err(error) => panic!("bind should succeed: {error}"),
    }
}

fn is_socket_permission_error(error: &TransportError) -> bool {
    matches!(error, TransportError::EndpointBind(bind_error) if {
        let message = bind_error.to_string();
        message.contains("Operation not permitted") || message.contains("Failed to bind sockets")
    })
}

fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("skyffla-transport-{name}-{nanos}"))
}

#[tokio::test]
async fn direct_file_transfer_downloads_from_peer() {
    let Some(host) = bind_or_skip().await else {
        return;
    };
    let Some(joiner) = bind_or_skip().await else {
        return;
    };
    let source = unique_temp_path("direct-source");
    let target = unique_temp_path("direct-target");
    std::fs::write(&source, b"hello direct peer").expect("source file should be writable");

    let prepared = host
        .prepare_file_path_with_progress(&source, |_| {})
        .await
        .expect("host should prepare direct file");
    host.register_outgoing_file(
        "c1",
        &source,
        prepared.size,
        prepared.display_name.clone(),
        Some(prepared.content_hash.clone()),
        None,
    )
    .await;
    let host_ticket = host.local_ticket().expect("host ticket should encode");

    let server = host.clone();
    let serve_task = tokio::spawn(async move {
        let connection = server
            .accept_transfer_connection()
            .await
            .expect("host should accept transfer connection");
        server.serve_registered_transfer(connection).await
    });

    let size = joiner
        .receive_file_with_progress(&host_ticket, "c1", &prepared.content_hash, &target, |_| {})
        .await
        .expect("joiner should receive direct file");
    match serve_task.await.expect("serve task should complete") {
        Ok(()) => {}
        Err(TransportError::DirectFileSend(message)) if message.contains("closed stream") => {}
        Err(error) => panic!("host should serve registered file: {error:?}"),
    }

    assert_eq!(size, 17);
    assert_eq!(
        std::fs::read(&target).expect("target should exist"),
        b"hello direct peer"
    );

    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&target);
    host.close().await;
    joiner.close().await;
}
