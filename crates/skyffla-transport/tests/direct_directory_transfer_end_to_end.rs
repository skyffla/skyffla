use std::time::{SystemTime, UNIX_EPOCH};

use skyffla_transport::{IrohTransport, TransportError, DEFAULT_DIRECTORY_TRANSFER_WORKERS};

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
async fn direct_directory_transfer_downloads_from_peer() {
    let Some(host) = bind_or_skip().await else {
        return;
    };
    let Some(joiner) = bind_or_skip().await else {
        return;
    };
    let source_dir = unique_temp_path("direct-dir-source");
    let target_dir = unique_temp_path("direct-dir-target");
    std::fs::create_dir_all(source_dir.join("nested")).expect("source dir should be writable");
    std::fs::create_dir_all(source_dir.join("empty")).expect("empty dir should be writable");
    std::fs::write(source_dir.join("a.txt"), b"alpha").expect("a.txt should be writable");
    std::fs::write(source_dir.join("nested").join("b.txt"), b"beta")
        .expect("b.txt should be writable");

    let prepared = host
        .prepare_directory_path_with_progress(&source_dir, |_| {})
        .await
        .expect("host should prepare direct directory");
    host.register_outgoing_directory("c2", &source_dir, prepared.clone(), None)
        .await;
    let host_ticket = host.local_ticket().expect("host ticket should encode");
    let server = host.clone();
    let serve_task = tokio::spawn(async move {
        let connection = server
            .accept_transfer_connection()
            .await
            .expect("host should accept transfer connection");
        server
            .serve_registered_transfer(connection)
            .await
            .expect("host should serve registered directory");
    });

    let size = joiner
        .receive_directory_with_progress(
            &host_ticket,
            "c2",
            &target_dir,
            DEFAULT_DIRECTORY_TRANSFER_WORKERS,
            |_| {},
        )
        .await
        .expect("joiner should receive direct directory");
    serve_task.await.expect("serve task should complete");

    assert_eq!(size, 9);
    assert_eq!(
        std::fs::read(target_dir.join("a.txt")).expect("a.txt should exist"),
        b"alpha"
    );
    assert_eq!(
        std::fs::read(target_dir.join("nested").join("b.txt")).expect("nested b.txt should exist"),
        b"beta"
    );
    assert!(target_dir.join("empty").is_dir());

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
    host.close().await;
    joiner.close().await;
}
