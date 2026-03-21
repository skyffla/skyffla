use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use iroh::endpoint::{Connection, ReadError, ReadExactError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode, TransportAddr};
use serde::{Deserialize, Serialize};
use skyffla_protocol::room::TransferPhase;
use skyffla_protocol::TransportCapability;
use tokio::sync::{mpsc, Mutex};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

pub const SKYFFLA_ALPN: &[u8] = b"skyffla/native/1";
pub const SKYFFLA_TRANSFER_ALPN: &[u8] = b"skyffla/transfer/1";
const TRANSFER_CHUNK_BYTES: usize = 64 * 1024;
const TRANSFER_WINDOW_BYTES: u64 = 1024 * 1024;
const TRANSFER_CREDIT_GRANT_BYTES: u64 = 256 * 1024;
const DIRECTORY_TRANSFER_CONCURRENCY: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportMode {
    Direct,
    Relay,
    Unknown,
}

impl fmt::Display for TransportMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => write!(f, "p2p"),
            Self::Relay => write!(f, "relay"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionStatus {
    pub mode: TransportMode,
    pub remote_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTicket {
    pub encoded: String,
}

impl PeerTicket {
    pub fn from_endpoint_addr(addr: &EndpointAddr) -> Result<Self, TransportError> {
        Ok(Self {
            encoded: serde_json::to_string(addr).map_err(TransportError::TicketEncode)?,
        })
    }

    pub fn to_endpoint_addr(&self) -> Result<EndpointAddr, TransportError> {
        serde_json::from_str(&self.encoded).map_err(TransportError::TicketDecode)
    }
}

pub trait PeerTransport {
    type Connection;
    type Error;

    fn transport_capabilities(&self) -> Vec<TransportCapability>;
}

#[derive(Debug)]
struct TransportInner {
    endpoint: Endpoint,
    options: TransportOptions,
    incoming: Mutex<mpsc::UnboundedReceiver<IrohConnection>>,
    transfer_incoming: Mutex<mpsc::UnboundedReceiver<IrohConnection>>,
    accept_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    outgoing_transfers: Mutex<std::collections::BTreeMap<String, RegisteredTransfer>>,
}

#[derive(Debug, Clone)]
pub struct IrohTransport {
    inner: Arc<TransportInner>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransportOptions {
    pub local_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    pub size: u64,
    pub display_name: String,
    pub content_hash: String,
}

#[derive(Debug, Clone)]
struct RegisteredFile {
    path: PathBuf,
    prepared: PreparedFile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedDirectoryEntry {
    pub relative_path: String,
    pub size: u64,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDirectory {
    pub total_size: u64,
    pub display_name: String,
    pub transfer_digest: String,
    pub entries: Vec<PreparedDirectoryEntry>,
}

#[derive(Debug, Clone)]
struct RegisteredDirectory {
    root: PathBuf,
    prepared: PreparedDirectory,
}

#[derive(Debug, Clone)]
enum RegisteredTransfer {
    File(RegisteredFile),
    Directory(RegisteredDirectory),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTransferProgress {
    pub phase: TransferPhase,
    pub bytes_complete: u64,
    pub bytes_total: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TransferRequest {
    channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TransferResponse {
    ReadyFile {
        size: u64,
        content_hash: String,
    },
    ReadyDirectory {
        total_size: u64,
        entries: Vec<PreparedDirectoryEntry>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TransferReceiverMessage {
    Credit {
        bytes: u64,
    },
}

impl IrohTransport {
    pub async fn bind() -> Result<Self, TransportError> {
        Self::bind_with_options(TransportOptions::default()).await
    }

    pub async fn bind_with_options(options: TransportOptions) -> Result<Self, TransportError> {
        let mut builder = Endpoint::builder().alpns(vec![
            SKYFFLA_ALPN.to_vec(),
            SKYFFLA_TRANSFER_ALPN.to_vec(),
        ]);
        if options.local_only {
            builder = builder.relay_mode(RelayMode::Disabled);
        }
        let endpoint = builder.bind().await.map_err(TransportError::EndpointBind)?;
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (transfer_incoming_tx, transfer_incoming_rx) = mpsc::unbounded_channel();
        let accept_task = tokio::spawn(run_accept_loop(
            endpoint.clone(),
            incoming_tx,
            transfer_incoming_tx,
        ));

        Ok(Self {
            inner: Arc::new(TransportInner {
                endpoint,
                options,
                incoming: Mutex::new(incoming_rx),
                transfer_incoming: Mutex::new(transfer_incoming_rx),
                accept_task: Mutex::new(Some(accept_task)),
                outgoing_transfers: Mutex::new(std::collections::BTreeMap::new()),
            }),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    pub fn local_ticket(&self) -> Result<PeerTicket, TransportError> {
        let addr = filter_endpoint_addr(self.inner.endpoint.addr(), self.inner.options.local_only);
        if self.inner.options.local_only && addr.addrs.is_empty() {
            return Err(TransportError::NoLocalAddresses);
        }
        PeerTicket::from_endpoint_addr(&addr)
    }

    pub async fn accept_connection(&self) -> Result<IrohConnection, TransportError> {
        self.inner
            .incoming
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::EndpointClosed)
    }

    pub async fn accept_transfer_connection(&self) -> Result<IrohConnection, TransportError> {
        self.inner
            .transfer_incoming
            .lock()
            .await
            .recv()
            .await
            .ok_or(TransportError::EndpointClosed)
    }

    pub async fn connect(&self, ticket: &PeerTicket) -> Result<IrohConnection, TransportError> {
        let endpoint_addr =
            filter_endpoint_addr(ticket.to_endpoint_addr()?, self.inner.options.local_only);
        if self.inner.options.local_only && endpoint_addr.addrs.is_empty() {
            return Err(TransportError::NoLocalAddresses);
        }
        let connection = self
            .inner
            .endpoint
            .connect(endpoint_addr, SKYFFLA_ALPN)
            .await
            .map_err(TransportError::Connect)?;
        Ok(IrohConnection { connection })
    }

    pub async fn connect_transfer(
        &self,
        ticket: &PeerTicket,
    ) -> Result<IrohConnection, TransportError> {
        let endpoint_addr =
            filter_endpoint_addr(ticket.to_endpoint_addr()?, self.inner.options.local_only);
        if self.inner.options.local_only && endpoint_addr.addrs.is_empty() {
            return Err(TransportError::NoLocalAddresses);
        }
        let connection = self
            .inner
            .endpoint
            .connect(endpoint_addr, SKYFFLA_TRANSFER_ALPN)
            .await
            .map_err(TransportError::ConnectTransfer)?;
        Ok(IrohConnection { connection })
    }

    pub async fn prepare_file_path_with_progress(
        &self,
        path: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<PreparedFile, TransportError> {
        let path = path.as_ref();
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
        if !metadata.is_file() {
            return Err(TransportError::DirectFilePrepare(format!(
                "path {} is not a regular file",
                path.display()
            )));
        }

        let size = metadata.len();
        let display_name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut file = File::open(path)
            .await
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
        let mut hasher = blake3::Hasher::new();
        let mut bytes_complete = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Preparing,
            bytes_complete: 0,
            bytes_total: Some(size),
        });
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            bytes_complete = bytes_complete.saturating_add(read as u64);
            on_progress(LocalTransferProgress {
                phase: TransferPhase::Preparing,
                bytes_complete,
                bytes_total: Some(size),
            });
        }
        Ok(PreparedFile {
            size,
            display_name,
            content_hash: hasher.finalize().to_hex().to_string(),
        })
    }

    pub async fn prepare_directory_path_with_progress(
        &self,
        path: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<PreparedDirectory, TransportError> {
        let path = path.as_ref();
        if !path.is_dir() {
            return Err(TransportError::DirectFilePrepare(format!(
                "path {} is not a directory",
                path.display()
            )));
        }
        let entries = collect_collection_entries(path)?;
        let display_name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let total_size = entries
            .iter()
            .fold(0_u64, |acc, entry| acc.saturating_add(entry.size));
        let mut prepared_entries = Vec::with_capacity(entries.len());
        let mut completed = 0_u64;
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Preparing,
            bytes_complete: 0,
            bytes_total: Some(total_size),
        });
        for entry in entries {
            let content_hash = hash_file_with_progress(&entry.absolute_path, entry.size, |current| {
                on_progress(LocalTransferProgress {
                    phase: TransferPhase::Preparing,
                    bytes_complete: completed.saturating_add(current),
                    bytes_total: Some(total_size),
                });
            })
            .await?;
            completed = completed.saturating_add(entry.size);
            on_progress(LocalTransferProgress {
                phase: TransferPhase::Preparing,
                bytes_complete: completed,
                bytes_total: Some(total_size),
            });
            prepared_entries.push(PreparedDirectoryEntry {
                relative_path: collection_entry_name(&entry.relative_path)?,
                size: entry.size,
                content_hash,
            });
        }
        Ok(PreparedDirectory {
            total_size,
            display_name,
            transfer_digest: directory_transfer_digest(&prepared_entries),
            entries: prepared_entries,
        })
    }

    pub async fn register_outgoing_file(
        &self,
        channel_id: impl Into<String>,
        path: impl AsRef<Path>,
        prepared: PreparedFile,
    ) {
        self.inner.outgoing_transfers.lock().await.insert(
            channel_id.into(),
            RegisteredTransfer::File(RegisteredFile {
                path: path.as_ref().to_path_buf(),
                prepared,
            }),
        );
    }

    pub async fn register_outgoing_directory(
        &self,
        channel_id: impl Into<String>,
        root: impl AsRef<Path>,
        prepared: PreparedDirectory,
    ) {
        self.inner.outgoing_transfers.lock().await.insert(
            channel_id.into(),
            RegisteredTransfer::Directory(RegisteredDirectory {
                root: root.as_ref().to_path_buf(),
                prepared,
            }),
        );
    }

    pub async fn unregister_outgoing_transfer(&self, channel_id: &str) {
        self.inner.outgoing_transfers.lock().await.remove(channel_id);
    }

    pub async fn receive_file_with_progress(
        &self,
        provider: &PeerTicket,
        channel_id: &str,
        expected_hash: &str,
        target: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<u64, TransportError> {
        let connection = self.connect_transfer(provider).await?;
        self.enforce_connection_policy(&connection).await?;
        let (mut send, mut recv) = connection.open_data_stream().await?;
        write_transfer_message(
            &mut send,
            &TransferRequest {
                channel_id: channel_id.to_string(),
                entry_path: None,
            },
            "transfer request",
        )
        .await?;

        let response: Option<TransferResponse> =
            read_transfer_message(&mut recv, "transfer response").await?;
        let response = response.ok_or_else(|| {
            TransportError::DirectFileReceive(
                "peer closed transfer connection before responding".into(),
            )
        })?;
        let size = match response {
            TransferResponse::ReadyFile { size, content_hash } => {
                if content_hash != expected_hash {
                    return Err(TransportError::DirectFileReceive(format!(
                        "transfer hash mismatch for {channel_id}: expected {expected_hash}, provider advertised {content_hash}"
                    )));
                }
                size
            }
            TransferResponse::ReadyDirectory { .. } => {
                return Err(TransportError::DirectFileReceive(format!(
                    "transfer channel {channel_id} is a directory, not a file"
                )));
            }
            TransferResponse::Error { message } => {
                return Err(TransportError::DirectFileReceive(message));
            }
        };

        let target = target.as_ref();
        if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        }
        let temp_target = temp_target_path(target);
        let mut file = File::create(&temp_target)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        let mut hasher = blake3::Hasher::new();
        let mut bytes_complete = 0_u64;
        let mut credit_pending = 0_u64;
        let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
        let (mut credit_send, _) = connection.open_data_stream().await?;
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Downloading,
            bytes_complete: 0,
            bytes_total: Some(size),
        });
        write_transfer_credit(&mut credit_send, TRANSFER_WINDOW_BYTES, "receiver credit").await?;
        loop {
            let read = match tokio::io::AsyncReadExt::read(&mut recv, &mut buffer).await {
                Ok(read) => read,
                Err(_error) if bytes_complete >= size => 0,
                Err(error) => {
                    return Err(TransportError::DirectFileReceive(error.to_string()));
                }
            };
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .await
                .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
            hasher.update(&buffer[..read]);
            bytes_complete = bytes_complete.saturating_add(read as u64);
            credit_pending = credit_pending.saturating_add(read as u64);
            if credit_pending >= TRANSFER_CREDIT_GRANT_BYTES {
                write_transfer_credit(&mut credit_send, credit_pending, "receiver credit").await?;
                credit_pending = 0;
            }
            on_progress(LocalTransferProgress {
                phase: TransferPhase::Downloading,
                bytes_complete,
                bytes_total: Some(size),
            });
        }
        file.flush()
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        drop(file);

        if bytes_complete != size {
            let _ = tokio::fs::remove_file(&temp_target).await;
            return Err(TransportError::DirectFileReceive(format!(
                "transfer size mismatch for {channel_id}: expected {size} bytes, received {bytes_complete}"
            )));
        }
        let actual_hash = hasher.finalize().to_hex().to_string();
        if actual_hash != expected_hash {
            let _ = tokio::fs::remove_file(&temp_target).await;
            return Err(TransportError::DirectFileReceive(format!(
                "transfer hash mismatch for {channel_id}: expected {expected_hash}, received {actual_hash}"
            )));
        }
        let _ = credit_send.finish();
        let _ = send.finish();
        tokio::fs::rename(&temp_target, target)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        Ok(size)
    }

    pub async fn receive_directory_with_progress(
        &self,
        provider: &PeerTicket,
        channel_id: &str,
        expected_digest: &str,
        target: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<u64, TransportError> {
        let connection = self.connect_transfer(provider).await?;
        self.enforce_connection_policy(&connection).await?;
        let (mut send, mut recv) = connection.open_data_stream().await?;
        write_transfer_message(
            &mut send,
            &TransferRequest {
                channel_id: channel_id.to_string(),
                entry_path: None,
            },
            "transfer request",
        )
        .await?;

        let response: Option<TransferResponse> =
            read_transfer_message(&mut recv, "transfer response").await?;
        let response = response.ok_or_else(|| {
            TransportError::DirectFileReceive(
                "peer closed transfer connection before responding".into(),
            )
        })?;
        let (total_size, entries) = match response {
            TransferResponse::ReadyDirectory { total_size, entries } => (total_size, entries),
            TransferResponse::Error { message } => {
                return Err(TransportError::DirectFileReceive(message));
            }
            TransferResponse::ReadyFile { .. } => {
                return Err(TransportError::DirectFileReceive(format!(
                    "transfer channel {channel_id} is a file, not a directory"
                )));
            }
        };

        let target = target.as_ref();
        if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        }
        let temp_root = temp_target_path(target);
        tokio::fs::create_dir_all(&temp_root)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        drop(recv);
        drop(send);

        let mut total_complete = 0_u64;
        let mut completed_entries = Vec::with_capacity(entries.len());
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let mut join_set = tokio::task::JoinSet::new();
        let mut entry_index = 0_usize;
        let mut in_flight = 0_usize;

        on_progress(LocalTransferProgress {
            phase: TransferPhase::Downloading,
            bytes_complete: 0,
            bytes_total: Some(total_size),
        });

        while entry_index < entries.len() && in_flight < DIRECTORY_TRANSFER_CONCURRENCY {
            spawn_directory_entry_receive(
                &mut join_set,
                connection.clone(),
                channel_id.to_string(),
                temp_root.clone(),
                entries[entry_index].clone(),
                progress_tx.clone(),
            );
            entry_index += 1;
            in_flight += 1;
        }

        while in_flight > 0 {
            tokio::select! {
                Some(delta) = progress_rx.recv() => {
                    total_complete = total_complete.saturating_add(delta);
                    on_progress(LocalTransferProgress {
                        phase: TransferPhase::Downloading,
                        bytes_complete: total_complete,
                        bytes_total: Some(total_size),
                    });
                }
                result = join_set.join_next() => {
                    let Some(result) = result else {
                        break;
                    };
                    in_flight = in_flight.saturating_sub(1);
                    match result {
                        Ok(Ok(entry)) => {
                            completed_entries.push(entry);
                        }
                        Ok(Err(error)) => {
                            join_set.abort_all();
                            let _ = tokio::fs::remove_dir_all(&temp_root).await;
                            return Err(error);
                        }
                        Err(error) => {
                            join_set.abort_all();
                            let _ = tokio::fs::remove_dir_all(&temp_root).await;
                            return Err(TransportError::DirectFileReceive(format!(
                                "directory transfer task failed: {error}"
                            )));
                        }
                    }

                    if entry_index < entries.len() {
                        spawn_directory_entry_receive(
                            &mut join_set,
                            connection.clone(),
                            channel_id.to_string(),
                            temp_root.clone(),
                            entries[entry_index].clone(),
                            progress_tx.clone(),
                        );
                        entry_index += 1;
                        in_flight += 1;
                    }
                }
            }
        }

        while let Ok(delta) = progress_rx.try_recv() {
            total_complete = total_complete.saturating_add(delta);
            on_progress(LocalTransferProgress {
                phase: TransferPhase::Downloading,
                bytes_complete: total_complete,
                bytes_total: Some(total_size),
            });
        }

        completed_entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let actual_digest = directory_transfer_digest(&completed_entries);
        if actual_digest != expected_digest {
            let _ = tokio::fs::remove_dir_all(&temp_root).await;
            return Err(TransportError::DirectFileReceive(format!(
                "transfer digest mismatch for {channel_id}: expected {expected_digest}, received {actual_digest}"
            )));
        }

        tokio::fs::rename(&temp_root, target)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        Ok(total_size)
    }

    pub async fn serve_registered_transfer(
        &self,
        connection: IrohConnection,
    ) -> Result<(), TransportError> {
        let (mut send, mut recv) = connection.accept_data_stream().await?;
        let request: Option<TransferRequest> =
            read_transfer_message(&mut recv, "transfer request").await?;
        let Some(request) = request else {
            return Ok(());
        };
        let registered = self
            .inner
            .outgoing_transfers
            .lock()
            .await
            .get(&request.channel_id)
            .cloned();
        let Some(registered) = registered else {
            write_transfer_message(
                &mut send,
                &TransferResponse::Error {
                    message: format!("unknown transfer channel {}", request.channel_id),
                },
                "transfer response",
            )
            .await?;
            return Ok(());
        };

        match registered {
            RegisteredTransfer::File(registered) => {
                if request.entry_path.is_some() {
                    write_transfer_message(
                        &mut send,
                        &TransferResponse::Error {
                            message: format!(
                                "transfer channel {} is a file and does not accept directory entry requests",
                                request.channel_id
                            ),
                        },
                        "transfer response",
                    )
                    .await?;
                    return Ok(());
                }
                write_transfer_message(
                    &mut send,
                    &TransferResponse::ReadyFile {
                        size: registered.prepared.size,
                        content_hash: registered.prepared.content_hash.clone(),
                    },
                    "transfer response",
                )
                .await?;

                let (_, mut credit_recv) = connection.accept_data_stream().await?;
                let mut file = File::open(&registered.path)
                    .await
                    .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                serve_file_with_credit(&mut send, &mut credit_recv, &mut file).await?;
            }
            RegisteredTransfer::Directory(registered) => {
                if let Some(entry_path) = request.entry_path {
                    let Some(entry) = registered
                        .prepared
                        .entries
                        .iter()
                        .find(|entry| entry.relative_path == entry_path)
                    else {
                        write_transfer_message(
                            &mut send,
                            &TransferResponse::Error {
                                message: format!(
                                    "directory transfer channel {} has no entry {}",
                                    request.channel_id, entry_path
                                ),
                            },
                            "transfer response",
                        )
                        .await?;
                        return Ok(());
                    };
                    let relative_path = validated_collection_relative_path(&entry.relative_path)
                        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                    let source = registered.root.join(relative_path);
                    write_transfer_message(
                        &mut send,
                        &TransferResponse::ReadyFile {
                            size: entry.size,
                            content_hash: entry.content_hash.clone(),
                        },
                        "transfer response",
                    )
                    .await?;
                    let (_, mut credit_recv) = connection.accept_data_stream().await?;
                    let mut file = File::open(&source)
                        .await
                        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                    serve_file_with_credit(&mut send, &mut credit_recv, &mut file).await?;
                } else {
                    write_transfer_message(
                        &mut send,
                        &TransferResponse::ReadyDirectory {
                            total_size: registered.prepared.total_size,
                            entries: registered.prepared.entries.clone(),
                        },
                        "transfer response",
                    )
                    .await?;
                    send.finish()
                        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                    let mut join_set = tokio::task::JoinSet::new();
                    for _ in 0..registered.prepared.entries.len() {
                        let (entry_send, entry_recv) = connection.accept_data_stream().await?;
                        let registered = registered.clone();
                        join_set.spawn(async move {
                            serve_directory_entry_stream(registered, entry_send, entry_recv).await
                        });
                    }
                    while let Some(result) = join_set.join_next().await {
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => return Err(error),
                            Err(error) => {
                                return Err(TransportError::DirectFileSend(format!(
                                    "directory stream task failed: {error}"
                                )));
                            }
                        }
                    }
                    return Ok(());
                }
            }
        }
        send.finish()
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        Ok(())
    }

    pub async fn enforce_connection_policy(
        &self,
        connection: &IrohConnection,
    ) -> Result<(), TransportError> {
        if !self.inner.options.local_only {
            return Ok(());
        }

        let status = self.connection_status(connection).await;
        if status.mode != TransportMode::Direct {
            return Err(TransportError::LocalModeRequiresDirectConnection { mode: status.mode });
        }

        let remote_addr = status
            .remote_addr
            .ok_or(TransportError::MissingRemoteAddrForLocalMode)?;
        let remote_addr: SocketAddr = remote_addr
            .parse()
            .map_err(|_| TransportError::InvalidRemoteAddrForLocalMode(remote_addr.clone()))?;
        if !is_local_ip(remote_addr.ip()) {
            return Err(TransportError::NonLocalPeerAddr(remote_addr));
        }

        Ok(())
    }

    pub async fn connection_status(&self, connection: &IrohConnection) -> ConnectionStatus {
        let remote_id = connection.connection.remote_id();
        let selected_path = connection.connection.to_info().selected_path();
        let mode = match selected_path.as_ref() {
            Some(path) if path.is_ip() => TransportMode::Direct,
            Some(path) if path.is_relay() => TransportMode::Relay,
            Some(_) | None => TransportMode::Unknown,
        };

        let selected_ip = selected_path
            .as_ref()
            .and_then(|path| match path.remote_addr() {
                TransportAddr::Ip(addr) => Some(addr.to_string()),
                TransportAddr::Relay(_) => None,
                _ => None,
            });

        let remote_info_ip = self
            .inner
            .endpoint
            .remote_info(remote_id)
            .await
            .and_then(|info| {
                info.into_addrs()
                    .find_map(|addr_info| match addr_info.into_addr() {
                        TransportAddr::Ip(addr) => Some(addr.to_string()),
                        TransportAddr::Relay(_) => None,
                        _ => None,
                    })
            });

        ConnectionStatus {
            mode,
            remote_addr: selected_ip.or(remote_info_ip),
        }
    }

    pub async fn close(&self) {
        self.inner.endpoint.close().await;
        if let Some(handle) = self.inner.accept_task.lock().await.take() {
            handle.abort();
        }
    }
}

#[derive(Debug, Clone)]
struct CollectionEntry {
    relative_path: PathBuf,
    absolute_path: PathBuf,
    size: u64,
}

fn collect_collection_entries(root: &Path) -> Result<Vec<CollectionEntry>, TransportError> {
    let mut entries = Vec::new();
    collect_collection_entries_recursive(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

fn collect_collection_entries_recursive(
    root: &Path,
    current: &Path,
    entries: &mut Vec<CollectionEntry>,
) -> Result<(), TransportError> {
    for entry in
        std::fs::read_dir(current)
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
    {
        let entry = entry.map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;

        if file_type.is_dir() {
            collect_collection_entries_recursive(root, &path, entries)?;
            continue;
        }
        if !(file_type.is_file() || file_type.is_symlink()) {
            continue;
        }

        let relative_path = path
            .strip_prefix(root)
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
            .to_path_buf();
        let size = std::fs::metadata(&path)
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
            .len();
        entries.push(CollectionEntry {
            relative_path,
            absolute_path: path,
            size,
        });
    }
    Ok(())
}

fn collection_entry_name(relative_path: &Path) -> Result<String, TransportError> {
    let relative_path = validated_collection_relative_path(relative_path)?;
    Ok(relative_path.to_string_lossy().replace('\\', "/"))
}

fn directory_transfer_digest(entries: &[PreparedDirectoryEntry]) -> String {
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(b"file\0");
        hasher.update(entry.relative_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.size.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.content_hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

async fn receive_directory_entry(
    connection: IrohConnection,
    channel_id: String,
    temp_root: PathBuf,
    entry: PreparedDirectoryEntry,
    progress_tx: mpsc::UnboundedSender<u64>,
) -> Result<PreparedDirectoryEntry, TransportError> {
    let (mut send, mut recv) = connection.open_data_stream().await?;
    write_transfer_message(
        &mut send,
        &TransferRequest {
            channel_id,
            entry_path: Some(entry.relative_path.clone()),
        },
        "transfer request",
    )
    .await?;

    let response: Option<TransferResponse> =
        read_transfer_message(&mut recv, "transfer response").await?;
    let response = response.ok_or_else(|| {
        TransportError::DirectFileReceive(
            "peer closed transfer connection before responding".into(),
        )
    })?;
    match response {
        TransferResponse::ReadyFile { size, content_hash } => {
            if size != entry.size {
                return Err(TransportError::DirectFileReceive(format!(
                    "directory entry {} size mismatch: expected {}, provider advertised {}",
                    entry.relative_path, entry.size, size
                )));
            }
            if content_hash != entry.content_hash {
                return Err(TransportError::DirectFileReceive(format!(
                    "directory entry {} hash mismatch: expected {}, provider advertised {}",
                    entry.relative_path, entry.content_hash, content_hash
                )));
            }
        }
        TransferResponse::ReadyDirectory { .. } => {
            return Err(TransportError::DirectFileReceive(format!(
                "directory entry request for {} returned a directory manifest",
                entry.relative_path
            )));
        }
        TransferResponse::Error { message } => {
            return Err(TransportError::DirectFileReceive(message));
        }
    }

    let relative_path = validated_collection_relative_path(&entry.relative_path)
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    let destination = temp_root.join(&relative_path);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    }
    let mut file = File::create(&destination)
        .await
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    write_transfer_credit(&mut send, TRANSFER_WINDOW_BYTES, "receiver credit").await?;

    let mut hasher = blake3::Hasher::new();
    let mut bytes_complete = 0_u64;
    let mut credit_pending = 0_u64;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    while bytes_complete < entry.size {
        let chunk_len = (entry.size - bytes_complete).min(buffer.len() as u64) as usize;
        let read = match tokio::io::AsyncReadExt::read(&mut recv, &mut buffer[..chunk_len]).await {
            Ok(0) => {
                return Err(TransportError::DirectFileReceive(format!(
                    "transfer channel closed early while receiving {}",
                    entry.relative_path
                )));
            }
            Ok(read) => read,
            Err(error) => {
                return Err(TransportError::DirectFileReceive(error.to_string()));
            }
        };
        file.write_all(&buffer[..read])
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        hasher.update(&buffer[..read]);
        bytes_complete = bytes_complete.saturating_add(read as u64);
        credit_pending = credit_pending.saturating_add(read as u64);
        let _ = progress_tx.send(read as u64);
        if credit_pending >= TRANSFER_CREDIT_GRANT_BYTES {
            write_transfer_credit(&mut send, credit_pending, "receiver credit").await?;
            credit_pending = 0;
        }
    }
    file.flush()
        .await
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    let actual_hash = hasher.finalize().to_hex().to_string();
    if actual_hash != entry.content_hash {
        return Err(TransportError::DirectFileReceive(format!(
            "transfer hash mismatch for {}: expected {}, received {}",
            entry.relative_path, entry.content_hash, actual_hash
        )));
    }
    let _ = send.finish();
    Ok(PreparedDirectoryEntry {
        relative_path: entry.relative_path,
        size: entry.size,
        content_hash: actual_hash,
    })
}

fn spawn_directory_entry_receive(
    join_set: &mut tokio::task::JoinSet<Result<PreparedDirectoryEntry, TransportError>>,
    connection: IrohConnection,
    channel_id: String,
    temp_root: PathBuf,
    entry: PreparedDirectoryEntry,
    progress_tx: mpsc::UnboundedSender<u64>,
) {
    join_set.spawn(async move {
        receive_directory_entry(connection, channel_id, temp_root, entry, progress_tx).await
    });
}

async fn serve_file_with_credit(
    send: &mut SendStream,
    credit_recv: &mut RecvStream,
    file: &mut File,
) -> Result<(), TransportError> {
    let mut available_credit = read_transfer_credit(credit_recv, "receiver credit").await?;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    loop {
        if available_credit == 0 {
            available_credit =
                available_credit.saturating_add(read_transfer_credit(credit_recv, "receiver credit").await?);
            continue;
        }
        let chunk_len = available_credit.min(buffer.len() as u64) as usize;
        let read = file
            .read(&mut buffer[..chunk_len])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        if read == 0 {
            break;
        }
        send.write_all(&buffer[..read])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        available_credit = available_credit.saturating_sub(read as u64);
    }
    Ok(())
}

async fn serve_file_with_credit_on_stream(
    send: &mut SendStream,
    recv: &mut RecvStream,
    file: &mut File,
) -> Result<(), TransportError> {
    let mut available_credit = read_transfer_credit(recv, "receiver credit").await?;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    loop {
        if available_credit == 0 {
            available_credit =
                available_credit.saturating_add(read_transfer_credit(recv, "receiver credit").await?);
            continue;
        }
        let chunk_len = available_credit.min(buffer.len() as u64) as usize;
        let read = file
            .read(&mut buffer[..chunk_len])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        if read == 0 {
            break;
        }
        send.write_all(&buffer[..read])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        available_credit = available_credit.saturating_sub(read as u64);
    }
    Ok(())
}

async fn serve_directory_entry_stream(
    registered: RegisteredDirectory,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<(), TransportError> {
    let request: Option<TransferRequest> =
        read_transfer_message(&mut recv, "transfer request").await?;
    let Some(request) = request else {
        return Ok(());
    };
    let Some(entry_path) = request.entry_path else {
        write_transfer_message(
            &mut send,
            &TransferResponse::Error {
                message: format!(
                    "directory transfer channel {} requires an entry path for data streams",
                    request.channel_id
                ),
            },
            "transfer response",
        )
        .await?;
        send.finish()
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        return Ok(());
    };
    let Some(entry) = registered
        .prepared
        .entries
        .iter()
        .find(|entry| entry.relative_path == entry_path)
    else {
        write_transfer_message(
            &mut send,
            &TransferResponse::Error {
                message: format!(
                    "directory transfer channel {} has no entry {}",
                    request.channel_id, entry_path
                ),
            },
            "transfer response",
        )
        .await?;
        send.finish()
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        return Ok(());
    };

    let relative_path = validated_collection_relative_path(&entry.relative_path)
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    let source = registered.root.join(relative_path);
    write_transfer_message(
        &mut send,
        &TransferResponse::ReadyFile {
            size: entry.size,
            content_hash: entry.content_hash.clone(),
        },
        "transfer response",
    )
    .await?;
    let mut file = File::open(&source)
        .await
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    serve_file_with_credit_on_stream(&mut send, &mut recv, &mut file).await?;
    send.finish()
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    Ok(())
}

fn validated_collection_relative_path(path: impl AsRef<Path>) -> Result<PathBuf, TransportError> {
    let path = path.as_ref();
    if path.is_absolute() {
        return Err(TransportError::DirectFilePrepare(format!(
            "collection entry {} must be relative",
            path.display()
        )));
    }

    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => clean.push(segment),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(TransportError::DirectFilePrepare(format!(
                    "collection entry {} contains invalid path components",
                    path.display()
                )));
            }
        }
    }

    if clean.as_os_str().is_empty() {
        return Err(TransportError::DirectFilePrepare(
            "collection entry path must not be empty".into(),
        ));
    }

    Ok(clean)
}

fn temp_target_path(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "transfer".into());
    target.with_file_name(format!("{file_name}.skyffla.part"))
}

async fn hash_file_with_progress(
    path: &Path,
    size: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<String, TransportError> {
    let mut file = File::open(path)
        .await
        .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes_complete = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes_complete = bytes_complete.saturating_add(read as u64);
        on_progress(bytes_complete.min(size));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn write_transfer_message<T>(
    send: &mut SendStream,
    value: &T,
    label: &str,
) -> Result<(), TransportError>
where
    T: Serialize,
{
    let bytes = skyffla_protocol::encode_frame(value)
        .map_err(|error| TransportError::TransferProtocol(error.to_string()))?;
    send.write_all(&bytes)
        .await
        .map_err(|error| TransportError::TransferProtocol(format!("failed to write {label}: {error}")))?;
    send.flush()
        .await
        .map_err(|error| TransportError::TransferProtocol(format!("failed to flush {label}: {error}")))?;
    Ok(())
}

async fn read_transfer_message<T>(
    recv: &mut RecvStream,
    label: &str,
) -> Result<Option<T>, TransportError>
where
    T: for<'de> Deserialize<'de>,
{
    let mut prefix = [0_u8; 4];
    match recv.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(bytes_read)) => {
            return Err(TransportError::TransferProtocol(format!(
                "peer closed {label} mid-frame after {bytes_read} bytes"
            )))
        }
        Err(ReadExactError::ReadError(ReadError::ClosedStream))
        | Err(ReadExactError::ReadError(ReadError::ConnectionLost(_))) => return Ok(None),
        Err(ReadExactError::ReadError(error)) => {
            return Err(TransportError::TransferProtocol(format!(
                "failed to read {label} prefix: {error}"
            )))
        }
    }
    let payload_len = u32::from_be_bytes(prefix) as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&prefix);
    frame.resize(4 + payload_len, 0);
    recv.read_exact(&mut frame[4..])
        .await
        .map_err(|error| TransportError::TransferProtocol(format!(
            "failed to read {label} payload: {error}"
        )))?;
    skyffla_protocol::decode_frame(&frame)
        .map(Some)
        .map_err(|error| TransportError::TransferProtocol(error.to_string()))
}

async fn write_transfer_credit(
    send: &mut SendStream,
    bytes: u64,
    label: &str,
) -> Result<(), TransportError> {
    if bytes == 0 {
        return Ok(());
    }
    write_transfer_message(send, &TransferReceiverMessage::Credit { bytes }, label).await
}

async fn read_transfer_credit(
    recv: &mut RecvStream,
    label: &str,
) -> Result<u64, TransportError> {
    let message: Option<TransferReceiverMessage> = read_transfer_message(recv, label).await?;
    let message = message.ok_or_else(|| {
        TransportError::TransferProtocol(format!("peer closed {label} stream before granting credit"))
    })?;
    match message {
        TransferReceiverMessage::Credit { bytes } if bytes > 0 => Ok(bytes),
        TransferReceiverMessage::Credit { .. } => Err(TransportError::TransferProtocol(format!(
            "peer sent zero-byte {label}"
        ))),
    }
}

async fn run_accept_loop(
    endpoint: Endpoint,
    incoming_tx: mpsc::UnboundedSender<IrohConnection>,
    transfer_incoming_tx: mpsc::UnboundedSender<IrohConnection>,
) {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            break;
        };
        let mut accepting = match incoming.accept() {
            Ok(accepting) => accepting,
            Err(_) => continue,
        };
        let alpn = match accepting.alpn().await {
            Ok(alpn) => alpn,
            Err(_) => continue,
        };
        let connection = match accepting.await {
            Ok(connection) => connection,
            Err(_) => continue,
        };
        match alpn.as_slice() {
            SKYFFLA_ALPN => {
                if incoming_tx.send(IrohConnection { connection }).is_err() {
                    break;
                }
            }
            SKYFFLA_TRANSFER_ALPN => {
                if transfer_incoming_tx
                    .send(IrohConnection { connection })
                    .is_err()
                {
                    break;
                }
            }
            _ => {}
        }
    }
}

impl PeerTransport for IrohTransport {
    type Connection = IrohConnection;
    type Error = TransportError;

    fn transport_capabilities(&self) -> Vec<TransportCapability> {
        vec![TransportCapability::NativeDirect]
    }
}

#[derive(Debug, Clone)]
pub struct IrohConnection {
    connection: Connection,
}

impl IrohConnection {
    pub fn remote_node_id(&self) -> String {
        self.connection.remote_id().to_string()
    }

    pub async fn open_control_stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        self.connection
            .open_bi()
            .await
            .map_err(TransportError::OpenBi)
    }

    pub async fn accept_control_stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        self.connection
            .accept_bi()
            .await
            .map_err(TransportError::AcceptBi)
    }

    pub async fn open_data_stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        self.open_control_stream().await
    }

    pub async fn accept_data_stream(&self) -> Result<(SendStream, RecvStream), TransportError> {
        self.accept_control_stream().await
    }

    pub fn transport_mode(&self) -> TransportMode {
        match self.connection.to_info().selected_path() {
            Some(path) if path.is_ip() => TransportMode::Direct,
            Some(path) if path.is_relay() => TransportMode::Relay,
            Some(_) | None => TransportMode::Unknown,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    EndpointBind(iroh::endpoint::BindError),
    EndpointClosed,
    Connect(iroh::endpoint::ConnectError),
    ConnectTransfer(iroh::endpoint::ConnectError),
    OpenBi(iroh::endpoint::ConnectionError),
    AcceptBi(iroh::endpoint::ConnectionError),
    TicketEncode(serde_json::Error),
    TicketDecode(serde_json::Error),
    DirectFilePrepare(String),
    DirectFileSend(String),
    DirectFileReceive(String),
    TransferProtocol(String),
    NoLocalAddresses,
    LocalModeRequiresDirectConnection { mode: TransportMode },
    MissingRemoteAddrForLocalMode,
    InvalidRemoteAddrForLocalMode(String),
    NonLocalPeerAddr(SocketAddr),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointBind(error) => write!(f, "failed to bind iroh endpoint: {error}"),
            Self::EndpointClosed => write!(f, "iroh endpoint closed before accepting a connection"),
            Self::Connect(error) => write!(f, "failed to connect via iroh: {error}"),
            Self::ConnectTransfer(error) => {
                write!(f, "failed to connect for direct transfer: {error}")
            }
            Self::OpenBi(error) => write!(f, "failed to open bidirectional stream: {error}"),
            Self::AcceptBi(error) => write!(f, "failed to accept bidirectional stream: {error}"),
            Self::TicketEncode(error) => write!(f, "failed to encode peer ticket: {error}"),
            Self::TicketDecode(error) => write!(f, "failed to decode peer ticket: {error}"),
            Self::DirectFilePrepare(error) => {
                write!(f, "failed to prepare direct file transfer: {error}")
            }
            Self::DirectFileSend(error) => write!(f, "failed to send direct file: {error}"),
            Self::DirectFileReceive(error) => {
                write!(f, "failed to receive direct file: {error}")
            }
            Self::TransferProtocol(error) => {
                write!(f, "direct transfer protocol error: {error}")
            }
            Self::NoLocalAddresses => write!(
                f,
                "local mode requires a peer ticket with at least one local-network address"
            ),
            Self::LocalModeRequiresDirectConnection { mode } => {
                write!(f, "local mode requires a direct p2p connection, got {mode}")
            }
            Self::MissingRemoteAddrForLocalMode => {
                write!(
                    f,
                    "local mode could not determine the peer's remote address"
                )
            }
            Self::InvalidRemoteAddrForLocalMode(addr) => {
                write!(
                    f,
                    "local mode could not parse the peer remote address {addr}"
                )
            }
            Self::NonLocalPeerAddr(addr) => {
                write!(f, "local mode rejected non-local peer address {addr}")
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EndpointBind(error) => Some(error),
            Self::Connect(error) => Some(error),
            Self::ConnectTransfer(error) => Some(error),
            Self::OpenBi(error) => Some(error),
            Self::AcceptBi(error) => Some(error),
            Self::TicketEncode(error) => Some(error),
            Self::TicketDecode(error) => Some(error),
            Self::EndpointClosed
            | Self::DirectFilePrepare(_)
            | Self::DirectFileSend(_)
            | Self::DirectFileReceive(_)
            | Self::TransferProtocol(_)
            | Self::NoLocalAddresses
            | Self::LocalModeRequiresDirectConnection { .. }
            | Self::MissingRemoteAddrForLocalMode
            | Self::InvalidRemoteAddrForLocalMode(_)
            | Self::NonLocalPeerAddr(_) => None,
        }
    }
}

fn filter_endpoint_addr(mut addr: EndpointAddr, local_only: bool) -> EndpointAddr {
    if !local_only {
        return addr;
    }
    addr.addrs.retain(|transport_addr| match transport_addr {
        TransportAddr::Ip(socket_addr) => is_local_ip(socket_addr.ip()),
        TransportAddr::Relay(_) => false,
        _ => false,
    });
    addr
}

fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000) == 0b0100_0000
                || ip.octets()[0] == 169 && ip.octets()[1] == 254
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV6};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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
    async fn peer_ticket_round_trip_preserves_endpoint_addr() {
        let Some(transport) = bind_or_skip().await else {
            return;
        };
        let ticket = transport
            .local_ticket()
            .expect("ticket encoding should succeed");
        let decoded = ticket
            .to_endpoint_addr()
            .expect("ticket decoding should succeed");

        assert_eq!(decoded.id, transport.endpoint().id());

        transport.close().await;
    }

    #[tokio::test]
    async fn iroh_transport_connects_and_exchanges_bytes() {
        let Some(host) = bind_or_skip().await else {
            return;
        };
        let Some(joiner) = bind_or_skip().await else {
            return;
        };
        let ticket = host.local_ticket().expect("host ticket should encode");
        let host_server = host.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let accept_task = tokio::spawn(async move {
            let connection = host_server
                .accept_connection()
                .await
                .expect("host should accept connection");
            let status = host_server.connection_status(&connection).await;
            assert_eq!(status.mode, TransportMode::Direct);
            assert!(status.remote_addr.is_some());
            let (mut send, mut recv) = connection
                .accept_control_stream()
                .await
                .expect("host should accept control stream");

            let payload = recv
                .read_to_end(1024)
                .await
                .expect("host should read control payload");
            assert_eq!(payload, b"hello from joiner");

            send.write_all(b"hello from host")
                .await
                .expect("host should write response");
            send.finish().expect("host should finish stream");
            let _ = done_rx.await;
        });

        let connection = joiner
            .connect(&ticket)
            .await
            .expect("joiner should connect");
        let status = joiner.connection_status(&connection).await;
        assert_eq!(status.mode, TransportMode::Direct);
        assert!(status.remote_addr.is_some());
        let (mut send, mut recv) = connection
            .open_control_stream()
            .await
            .expect("joiner should open control stream");

        send.write_all(b"hello from joiner")
            .await
            .expect("joiner should write payload");
        send.finish().expect("joiner should finish stream");

        let payload = recv
            .read_to_end(1024)
            .await
            .expect("joiner should read response");
        assert_eq!(payload, b"hello from host");

        let _ = done_tx.send(());
        accept_task.await.expect("accept task should join");
        host.close().await;
        joiner.close().await;
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
        host.register_outgoing_file("c1", &source, prepared.clone()).await;
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
                .expect("host should serve registered file");
        });

        let size = joiner
            .receive_file_with_progress(&host_ticket, "c1", &prepared.content_hash, &target, |_| {})
            .await
            .expect("joiner should receive direct file");
        serve_task.await.expect("serve task should complete");

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
        std::fs::create_dir_all(source_dir.join("nested"))
            .expect("source dir should be writable");
        std::fs::write(source_dir.join("a.txt"), b"alpha").expect("a.txt should be writable");
        std::fs::write(source_dir.join("nested").join("b.txt"), b"beta")
            .expect("b.txt should be writable");

        let prepared = host
            .prepare_directory_path_with_progress(&source_dir, |_| {})
            .await
            .expect("host should prepare direct directory");
        host.register_outgoing_directory("c2", &source_dir, prepared.clone())
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
                &prepared.transfer_digest,
                &target_dir,
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
            std::fs::read(target_dir.join("nested").join("b.txt"))
                .expect("nested b.txt should exist"),
            b"beta"
        );

        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
        host.close().await;
        joiner.close().await;
    }

    #[tokio::test]
    async fn local_mode_ticket_filter_keeps_only_local_ips() {
        let Some(transport) = bind_or_skip().await else {
            return;
        };
        let endpoint_addr = EndpointAddr::from_parts(
            transport.endpoint().id(),
            [
                TransportAddr::Ip("192.168.1.20:7777".parse().unwrap()),
                TransportAddr::Ip("8.8.8.8:7777".parse().unwrap()),
                TransportAddr::Ip(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::LOCALHOST,
                    7777,
                    0,
                    0,
                ))),
                TransportAddr::Relay("https://relay.example.com".parse().unwrap()),
            ],
        );

        let filtered = filter_endpoint_addr(endpoint_addr, true);
        assert_eq!(filtered.addrs.len(), 2);
        assert!(filtered
            .addrs
            .contains(&TransportAddr::Ip("192.168.1.20:7777".parse().unwrap())));
        assert!(filtered
            .addrs
            .contains(&TransportAddr::Ip(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::LOCALHOST,
                7777,
                0,
                0
            )))));

        transport.close().await;
    }

    #[test]
    fn local_ip_helper_accepts_private_and_rejects_public_ips() {
        assert!(is_local_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(is_local_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_local_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_local_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_local_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
    }
}
