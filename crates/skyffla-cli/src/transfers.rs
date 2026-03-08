use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arboard::Clipboard;
use iroh::endpoint::SendStream;
use sha2::{Digest as ShaDigestTrait, Sha256};
use skyffla_protocol::{
    Accept, Compression, ControlMessage, DataStreamHeader, Digest, Envelope, Offer, TransferKind,
};
use skyffla_transport::IrohConnection;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;
use tokio::task;

use crate::net::framing::{next_message_id, read_data_header, write_data_header, write_envelope};
use crate::ui::{TransferStateUi, TransferUi, UiState};

#[derive(Clone)]
pub(crate) struct FolderStats {
    pub(crate) item_count: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct PendingOutgoingTransfer {
    pub(crate) offer: Offer,
    pub(crate) source: OutgoingTransferSource,
    pub(crate) cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) enum OutgoingTransferSource {
    File {
        path: PathBuf,
        file_name: String,
        size: u64,
    },
    Folder {
        path: PathBuf,
        folder_name: String,
        stats: FolderStats,
    },
    Clipboard {
        text: String,
    },
}

pub(crate) enum TransferTaskEvent {
    Progress {
        transfer_id: String,
        bytes_done: u64,
        bytes_total: Option<u64>,
    },
    LocalComplete {
        transfer_id: String,
        message: String,
    },
    ReceiveComplete {
        transfer_id: String,
        message: String,
        digest: Digest,
    },
    Failed {
        transfer_id: String,
        message: String,
        notify_peer: bool,
    },
}

pub(crate) async fn send_path(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.is_dir() {
        send_folder(session_id, send, path, ui).await
    } else if metadata.is_file() {
        send_file(session_id, send, path, ui).await
    } else {
        bail!(
            "{} is neither a regular file nor a directory",
            path.display()
        )
    }
}

pub(crate) async fn send_clipboard(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let clipboard_text = read_clipboard_text().await?;
    let transfer_id = format!("t{}", next_message_id());
    let bytes = clipboard_text.as_bytes();
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::Clipboard,
        name: "clipboard".to_string(),
        size: Some(bytes.len() as u64),
        mime: Some("text/plain".to_string()),
        item_count: None,
        compression: None,
        path_hint: None,
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(bytes.len() as u64),
    });
    ui.system(format!("offered clipboard text ({} bytes)", bytes.len()));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::Clipboard {
            text: clipboard_text,
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

pub(crate) async fn accept_pending_offer(
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    download_dir: &Path,
    offer: Offer,
    ui: &mut UiState,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<Arc<AtomicBool>> {
    let item_name = offer.name.clone();
    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Accept(Accept {
                transfer_id: offer.transfer_id.clone(),
            }),
        ),
    )
    .await?;
    ui.mark_transfer_streaming(&offer.transfer_id);
    ui.system(format!(
        "accepting {} {}",
        transfer_kind_label(&offer.kind),
        item_name
    ));
    ui.render();

    fs::create_dir_all(download_dir)
        .await
        .with_context(|| format!("failed to create {}", download_dir.display()))?;
    let cancel = Arc::new(AtomicBool::new(false));
    spawn_incoming_transfer_task(
        connection.clone(),
        download_dir.to_path_buf(),
        offer,
        cancel.clone(),
        task_tx,
    );
    Ok(cancel)
}

pub(crate) fn spawn_outgoing_transfer_task(
    connection: IrohConnection,
    plan: PendingOutgoingTransfer,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) {
    task::spawn(async move {
        let result = match &plan.source {
            OutgoingTransferSource::File {
                path,
                file_name,
                size,
            } => {
                run_outgoing_file_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    path,
                    file_name,
                    *size,
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
            OutgoingTransferSource::Folder {
                path,
                folder_name,
                stats,
            } => {
                run_outgoing_folder_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    path,
                    folder_name,
                    stats.clone(),
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
            OutgoingTransferSource::Clipboard { text } => {
                run_outgoing_clipboard_transfer(
                    &connection,
                    &plan.offer.transfer_id,
                    text,
                    &plan.cancel,
                    &task_tx,
                )
                .await
            }
        };

        if let Err(error) = result {
            let _ = task_tx.send(TransferTaskEvent::Failed {
                transfer_id: plan.offer.transfer_id.clone(),
                message: format!("transfer {} failed: {error:#}", plan.offer.transfer_id),
                notify_peer: false,
            });
        }
    });
}

pub(crate) fn transfer_kind_label(kind: &TransferKind) -> &'static str {
    match kind {
        TransferKind::File => "file",
        TransferKind::FolderArchive => "folder",
        TransferKind::Clipboard => "clipboard",
        TransferKind::Stdio => "stdio",
    }
}

pub(crate) fn describe_offer_size(offer: &Offer) -> String {
    match (offer.kind.clone(), offer.size, offer.item_count) {
        (TransferKind::FolderArchive, Some(size), Some(items)) => {
            format!(" ({} items, {} bytes)", items, size)
        }
        (TransferKind::FolderArchive, _, Some(items)) => format!(" ({} items)", items),
        (_, Some(size), _) => format!(" ({} bytes)", size),
        _ => String::new(),
    }
}

pub(crate) fn finalize_sha256_digest(hasher: Sha256) -> Digest {
    let bytes = hasher.finalize();
    Digest {
        algorithm: "sha256".to_string(),
        value_hex: format!("{bytes:x}"),
    }
}

pub(crate) fn format_digest_suffix(digest: Option<&Digest>) -> String {
    digest
        .map(|digest| format!(" [{}:{}]", digest.algorithm, digest.value_hex))
        .unwrap_or_default()
}

async fn send_file(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("failed to stat {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("failed to derive file name from {}", path.display()))?;
    let transfer_id = format!("t{}", next_message_id());
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::File,
        name: file_name.clone(),
        size: Some(metadata.len()),
        mime: None,
        item_count: None,
        compression: None,
        path_hint: Some(file_name.clone()),
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(metadata.len()),
    });
    ui.system(format!("offered file {}", file_name));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::File {
            path: path.to_path_buf(),
            file_name,
            size: metadata.len(),
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

async fn send_folder(
    session_id: &str,
    send: &mut SendStream,
    path: &Path,
    ui: &mut UiState,
) -> Result<PendingOutgoingTransfer> {
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }

    let folder_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("failed to derive folder name from {}", path.display()))?;
    let stats = collect_folder_stats(path)?;
    let transfer_id = format!("t{}", next_message_id());
    let offer = Offer {
        transfer_id: transfer_id.clone(),
        kind: TransferKind::FolderArchive,
        name: folder_name.clone(),
        size: Some(stats.total_bytes),
        mime: None,
        item_count: Some(stats.item_count),
        compression: Some(Compression::Tar),
        path_hint: Some(folder_name.clone()),
    };

    write_envelope(
        send,
        &Envelope::new(
            session_id,
            next_message_id(),
            ControlMessage::Offer(offer.clone()),
        ),
    )
    .await?;
    ui.upsert_transfer(TransferUi {
        id: transfer_id.clone(),
        state: TransferStateUi::Pending,
        bytes_done: 0,
        bytes_total: Some(stats.total_bytes),
    });
    ui.system(format!(
        "offered folder {} ({} items, {} bytes)",
        folder_name, stats.item_count, stats.total_bytes
    ));
    ui.render();
    Ok(PendingOutgoingTransfer {
        offer,
        source: OutgoingTransferSource::Folder {
            path: path.to_path_buf(),
            folder_name,
            stats,
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
}

fn spawn_incoming_transfer_task(
    connection: IrohConnection,
    download_dir: PathBuf,
    offer: Offer,
    cancel: Arc<AtomicBool>,
    task_tx: mpsc::UnboundedSender<TransferTaskEvent>,
) {
    task::spawn(async move {
        if let Err(error) =
            run_incoming_transfer(connection, download_dir, offer.clone(), cancel, &task_tx).await
        {
            let _ = task_tx.send(TransferTaskEvent::Failed {
                transfer_id: offer.transfer_id.clone(),
                message: format!("receive failed for {}: {error:#}", offer.name),
                notify_peer: true,
            });
        }
    });
}

async fn run_outgoing_file_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    path: &Path,
    file_name: &str,
    size: u64,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::File,
        },
    )
    .await?;

    let mut file = File::open(path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let bytes_read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to stream file bytes")?;
        hasher.update(&buffer[..bytes_read]);
        bytes_done += bytes_read as u64;
        let _ = task_tx.send(TransferTaskEvent::Progress {
            transfer_id: transfer_id.to_string(),
            bytes_done,
            bytes_total: Some(size),
        });
    }
    if !cancel.load(Ordering::SeqCst) {
        data_send.finish().context("failed to finish data stream")?;
        let digest = finalize_sha256_digest(hasher);
        let _ = task_tx.send(TransferTaskEvent::LocalComplete {
            transfer_id: transfer_id.to_string(),
            message: format!(
                "sent file {}{}",
                file_name,
                format_digest_suffix(Some(&digest))
            ),
        });
    }
    Ok(())
}

async fn run_outgoing_folder_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    path: &Path,
    folder_name: &str,
    stats: FolderStats,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut child = TokioCommand::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(parent)
        .arg(folder_name)
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to start tar for folder transfer")?;
    let mut tar_stdout = child
        .stdout
        .take()
        .context("failed to capture tar stdout")?;

    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::FolderArchive,
        },
    )
    .await?;

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.start_kill();
            return Ok(());
        }
        let bytes_read = tar_stdout
            .read(&mut buffer)
            .await
            .context("failed to read tar output")?;
        if bytes_read == 0 {
            break;
        }
        data_send
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed to stream folder archive bytes")?;
        hasher.update(&buffer[..bytes_read]);
        bytes_done += bytes_read as u64;
        let _ = task_tx.send(TransferTaskEvent::Progress {
            transfer_id: transfer_id.to_string(),
            bytes_done,
            bytes_total: Some(stats.total_bytes),
        });
    }

    let status = child
        .wait()
        .await
        .context("failed waiting for tar sender")?;
    if !status.success() {
        bail!("tar sender exited with status {status}");
    }

    if !cancel.load(Ordering::SeqCst) {
        data_send.finish().context("failed to finish data stream")?;
        let digest = finalize_sha256_digest(hasher);
        let _ = task_tx.send(TransferTaskEvent::LocalComplete {
            transfer_id: transfer_id.to_string(),
            message: format!(
                "sent folder {}{}",
                folder_name,
                format_digest_suffix(Some(&digest))
            ),
        });
    }
    Ok(())
}

async fn run_outgoing_clipboard_transfer(
    connection: &IrohConnection,
    transfer_id: &str,
    text: &str,
    cancel: &Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let bytes = text.as_bytes();
    let (mut data_send, _) = connection.open_data_stream().await?;
    write_data_header(
        &mut data_send,
        &DataStreamHeader {
            transfer_id: transfer_id.to_string(),
            kind: TransferKind::Clipboard,
        },
    )
    .await?;

    if cancel.load(Ordering::SeqCst) {
        return Ok(());
    }
    data_send
        .write_all(bytes)
        .await
        .context("failed to stream clipboard bytes")?;
    data_send.finish().context("failed to finish data stream")?;
    let _ = task_tx.send(TransferTaskEvent::Progress {
        transfer_id: transfer_id.to_string(),
        bytes_done: bytes.len() as u64,
        bytes_total: Some(bytes.len() as u64),
    });
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = finalize_sha256_digest(hasher);
    let _ = task_tx.send(TransferTaskEvent::LocalComplete {
        transfer_id: transfer_id.to_string(),
        message: format!("sent clipboard text{}", format_digest_suffix(Some(&digest))),
    });
    Ok(())
}

async fn run_incoming_transfer(
    connection: IrohConnection,
    download_dir: PathBuf,
    offer: Offer,
    cancel: Arc<AtomicBool>,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
) -> Result<()> {
    let item_name = offer.name.clone();
    let (_, mut data_recv) = connection.accept_data_stream().await?;
    let header = read_data_header(&mut data_recv)
        .await?
        .context("peer closed data stream before sending a header")?;
    if header.transfer_id != offer.transfer_id || header.kind != offer.kind {
        bail!(
            "received unexpected data stream header for transfer {}",
            offer.transfer_id
        );
    }

    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes_done = 0_u64;
    let mut hasher = Sha256::new();
    let completion_label = match offer.kind.clone() {
        TransferKind::File => {
            let target_path = download_dir.join(&item_name);
            let mut output = File::create(&target_path)
                .await
                .with_context(|| format!("failed to create {}", target_path.display()))?;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        output
                            .write_all(&buffer[..bytes_read])
                            .await
                            .with_context(|| {
                                format!("failed to write {}", target_path.display())
                            })?;
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => return Err(error).context("failed to read incoming file stream"),
                }
            }
            target_path.display().to_string()
        }
        TransferKind::FolderArchive => {
            let mut child = TokioCommand::new("tar")
                .arg("-xf")
                .arg("-")
                .arg("-C")
                .arg(&download_dir)
                .stdin(Stdio::piped())
                .spawn()
                .context("failed to start tar for folder extraction")?;
            let mut tar_stdin = child.stdin.take().context("failed to capture tar stdin")?;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    let _ = child.start_kill();
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        tar_stdin
                            .write_all(&buffer[..bytes_read])
                            .await
                            .context("failed to write folder archive bytes to tar")?;
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(error).context("failed to read incoming folder stream");
                    }
                }
            }
            tar_stdin
                .shutdown()
                .await
                .context("failed to finish tar stdin")?;
            drop(tar_stdin);
            let status = child
                .wait()
                .await
                .context("failed waiting for tar extractor")?;
            if !status.success() {
                bail!("tar extractor exited with status {status}");
            }
            download_dir.join(&item_name).display().to_string()
        }
        TransferKind::Clipboard => {
            let mut data = Vec::new();
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Ok(());
                }
                match data_recv.read(&mut buffer).await {
                    Ok(Some(bytes_read)) => {
                        data.extend_from_slice(&buffer[..bytes_read]);
                        hasher.update(&buffer[..bytes_read]);
                        bytes_done += bytes_read as u64;
                        let _ = task_tx.send(TransferTaskEvent::Progress {
                            transfer_id: offer.transfer_id.clone(),
                            bytes_done,
                            bytes_total: offer.size,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return Err(error).context("failed to read incoming clipboard stream");
                    }
                }
            }
            let text =
                String::from_utf8(data).context("received clipboard payload was not UTF-8")?;
            write_clipboard_text(text).await?;
            "local clipboard".to_string()
        }
        _ => bail!("unsupported accepted transfer kind: {:?}", offer.kind),
    };

    let digest = finalize_sha256_digest(hasher);
    let _ = task_tx.send(TransferTaskEvent::ReceiveComplete {
        transfer_id: offer.transfer_id.clone(),
        message: format!(
            "received {} {}{}",
            transfer_kind_label(&offer.kind),
            completion_label,
            format_digest_suffix(Some(&digest))
        ),
        digest,
    });
    Ok(())
}

fn collect_folder_stats(path: &Path) -> Result<FolderStats> {
    fn visit(path: &Path, stats: &mut FolderStats) -> Result<()> {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("failed to read directory {}", path.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
            let entry_path = entry.path();
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat {}", entry_path.display()))?;
            stats.item_count += 1;
            if metadata.is_dir() {
                visit(&entry_path, stats)?;
            } else if metadata.is_file() {
                stats.total_bytes += metadata.len();
            }
        }
        Ok(())
    }

    let mut stats = FolderStats {
        item_count: 0,
        total_bytes: 0,
    };
    visit(path, &mut stats)?;
    Ok(stats)
}

async fn read_clipboard_text() -> Result<String> {
    task::spawn_blocking(|| {
        let mut clipboard = Clipboard::new().context("failed to access local clipboard")?;
        clipboard
            .get_text()
            .context("failed to read text from local clipboard")
    })
    .await
    .context("clipboard read task failed")?
}

async fn write_clipboard_text(text: String) -> Result<()> {
    task::spawn_blocking(move || {
        let mut clipboard = Clipboard::new().context("failed to access local clipboard")?;
        clipboard
            .set_text(text)
            .context("failed to write text to local clipboard")
    })
    .await
    .context("clipboard write task failed")?
}
