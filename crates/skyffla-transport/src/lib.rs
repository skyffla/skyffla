use std::fmt;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use iroh::endpoint::{Connection, ReadError, ReadExactError, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, RelayMode, TransportAddr};
use serde::{Deserialize, Serialize};
use skyffla_protocol::room::{TransferItemKind, TransferPhase};
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
pub const DEFAULT_DIRECTORY_TRANSFER_WORKERS: usize = 4;
const PREP_HASH_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const PREP_HASH_PROGRESS_GRANULARITY_BYTES: u64 = 8 * 1024 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFile {
    pub size: u64,
    pub content_hash: String,
    pub temp_path: PathBuf,
    pub final_path: PathBuf,
}

#[derive(Debug, Clone)]
struct RegisteredFile {
    path: PathBuf,
    size: u64,
    display_name: String,
    content_hash: Option<String>,
    progress_tx: Option<mpsc::UnboundedSender<TransferRuntimeProgress>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreparedDirectoryEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedDirectoryEntry {
    pub relative_path: String,
    pub entry_kind: PreparedDirectoryEntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDirectory {
    pub total_size: u64,
    pub display_name: String,
    pub entries: Vec<PreparedDirectoryEntry>,
}

#[derive(Debug, Clone)]
struct RegisteredDirectory {
    root: PathBuf,
    prepared: PreparedDirectory,
    progress_tx: Option<mpsc::UnboundedSender<TransferRuntimeProgress>>,
    aggregate_progress: Arc<std::sync::atomic::AtomicU64>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRuntimeProgress {
    pub channel_id: String,
    pub item_kind: TransferItemKind,
    pub name: String,
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

impl TransferRequest {
    fn manifest(channel_id: &str) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            entry_path: None,
        }
    }

    fn entry(channel_id: String, entry_path: String) -> Self {
        Self {
            channel_id,
            entry_path: Some(entry_path),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TransferResponse {
    ReadyFile {
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    ReadyDirectory {
        total_size: u64,
        entries: Vec<PreparedDirectoryEntry>,
    },
    FinalizedFile {
        content_hash: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TransferReceiverMessage {
    Credit { bytes: u64 },
}

impl IrohTransport {
    pub async fn bind() -> Result<Self, TransportError> {
        Self::bind_with_options(TransportOptions::default()).await
    }

    pub async fn bind_with_options(options: TransportOptions) -> Result<Self, TransportError> {
        let mut builder =
            Endpoint::builder().alpns(vec![SKYFFLA_ALPN.to_vec(), SKYFFLA_TRANSFER_ALPN.to_vec()]);
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
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Preparing,
            bytes_complete: 0,
            bytes_total: Some(size),
        });
        let content_hash = hash_file_with_progress(path, size, |bytes_complete| {
            on_progress(LocalTransferProgress {
                phase: TransferPhase::Preparing,
                bytes_complete,
                bytes_total: Some(size),
            });
        })
        .await?;
        Ok(PreparedFile {
            size,
            display_name,
            content_hash,
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
            .filter(|entry| entry.entry_kind == PreparedDirectoryEntryKind::File)
            .fold(0_u64, |acc, entry| acc.saturating_add(entry.size));
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Preparing,
            bytes_complete: 0,
            bytes_total: Some(total_size),
        });
        let prepared_entries = entries
            .into_iter()
            .map(|entry| {
                Ok(PreparedDirectoryEntry {
                    relative_path: collection_entry_name(&entry.relative_path)?,
                    entry_kind: entry.entry_kind,
                    size: entry.size,
                })
            })
            .collect::<Result<Vec<_>, TransportError>>()?;
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Preparing,
            bytes_complete: total_size,
            bytes_total: Some(total_size),
        });
        Ok(PreparedDirectory {
            total_size,
            display_name,
            entries: prepared_entries,
        })
    }

    pub async fn register_outgoing_file(
        &self,
        channel_id: impl Into<String>,
        path: impl AsRef<Path>,
        size: u64,
        display_name: impl Into<String>,
        content_hash: Option<String>,
        progress_tx: Option<mpsc::UnboundedSender<TransferRuntimeProgress>>,
    ) {
        self.inner.outgoing_transfers.lock().await.insert(
            channel_id.into(),
            RegisteredTransfer::File(RegisteredFile {
                path: path.as_ref().to_path_buf(),
                size,
                display_name: display_name.into(),
                content_hash,
                progress_tx,
            }),
        );
    }

    pub async fn update_outgoing_file_hash(
        &self,
        channel_id: &str,
        content_hash: String,
    ) -> Result<(), TransportError> {
        let mut transfers = self.inner.outgoing_transfers.lock().await;
        let Some(registered) = transfers.get_mut(channel_id) else {
            return Err(TransportError::DirectFileSend(format!(
                "unknown transfer channel {channel_id}"
            )));
        };
        match registered {
            RegisteredTransfer::File(file) => {
                file.content_hash = Some(content_hash);
                Ok(())
            }
            RegisteredTransfer::Directory(_) => Err(TransportError::DirectFileSend(format!(
                "transfer channel {channel_id} is a directory, not a file"
            ))),
        }
    }

    pub async fn register_outgoing_directory(
        &self,
        channel_id: impl Into<String>,
        root: impl AsRef<Path>,
        prepared: PreparedDirectory,
        progress_tx: Option<mpsc::UnboundedSender<TransferRuntimeProgress>>,
    ) {
        self.inner.outgoing_transfers.lock().await.insert(
            channel_id.into(),
            RegisteredTransfer::Directory(RegisteredDirectory {
                root: root.as_ref().to_path_buf(),
                prepared,
                progress_tx,
                aggregate_progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            }),
        );
    }

    pub async fn unregister_outgoing_transfer(&self, channel_id: &str) {
        self.inner
            .outgoing_transfers
            .lock()
            .await
            .remove(channel_id);
    }

    pub async fn receive_file_with_progress(
        &self,
        provider: &PeerTicket,
        channel_id: &str,
        expected_hash: &str,
        target: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<u64, TransportError> {
        let received = self
            .receive_file_to_temp_with_progress(provider, channel_id, None, target, |progress| {
                on_progress(progress);
            })
            .await?;
        if received.content_hash != expected_hash {
            let _ = tokio::fs::remove_file(&received.temp_path).await;
            return Err(TransportError::DirectFileReceive(format!(
                "transfer hash mismatch: expected {expected_hash}, received {}",
                received.content_hash
            )));
        }
        tokio::fs::rename(&received.temp_path, &received.final_path)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        Ok(received.size)
    }

    pub async fn receive_file_to_temp_with_progress(
        &self,
        provider: &PeerTicket,
        channel_id: &str,
        expected_size: Option<u64>,
        target: impl AsRef<Path>,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<ReceivedFile, TransportError> {
        let (connection, mut send, mut recv) =
            request_transfer_connection(self, provider, TransferRequest::manifest(channel_id))
                .await?;
        let size = expect_ready_file_response(
            read_transfer_response_required(&mut recv).await?,
            expected_size,
            channel_id,
        )?;

        let target = target.as_ref();
        if let Some(parent) = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        }
        let temp_target = temp_target_path(target);
        let (mut credit_send, _) = connection.open_data_stream().await?;
        on_progress(LocalTransferProgress {
            phase: TransferPhase::Downloading,
            bytes_complete: 0,
            bytes_total: Some(size),
        });
        let receive_result = receive_file_to_temp(
            &mut credit_send,
            &mut recv,
            size,
            &temp_target,
            |bytes_complete| {
                on_progress(LocalTransferProgress {
                    phase: TransferPhase::Downloading,
                    bytes_complete,
                    bytes_total: Some(size),
                });
            },
        )
        .await;
        let content_hash = match receive_result {
            Ok(content_hash) => content_hash,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temp_target).await;
                return Err(error);
            }
        };
        let _ = credit_send.finish();
        let _ = send.finish();
        Ok(ReceivedFile {
            size,
            content_hash,
            temp_path: temp_target,
            final_path: target.to_path_buf(),
        })
    }

    pub async fn receive_directory_with_progress(
        &self,
        provider: &PeerTicket,
        channel_id: &str,
        target: impl AsRef<Path>,
        worker_count: usize,
        mut on_progress: impl FnMut(LocalTransferProgress),
    ) -> Result<u64, TransportError> {
        let (connection, mut send, mut recv) =
            request_transfer_connection(self, provider, TransferRequest::manifest(channel_id))
                .await?;
        let (total_size, entries) = expect_ready_directory_response(
            read_transfer_response_required(&mut recv).await?,
            channel_id,
        )?;

        let target = target.as_ref();
        if let Some(parent) = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        }
        let temp_root = temp_target_path(target);
        tokio::fs::create_dir_all(&temp_root)
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        let _ = send.finish();
        drop(recv);

        receive_directory_entries(
            connection,
            channel_id,
            &temp_root,
            &entries,
            total_size,
            worker_count,
            &mut on_progress,
        )
        .await
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&temp_root);
        })?;

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
            write_transfer_error_response(
                &mut send,
                format!("unknown transfer channel {}", request.channel_id),
            )
            .await?;
            return Ok(());
        };

        match registered {
            RegisteredTransfer::File(registered) => {
                let file_send_result: Result<(), TransportError> = async {
                    if request.entry_path.is_some() {
                        write_transfer_error_response(
                            &mut send,
                            format!(
                                "transfer channel {} is a file and does not accept directory entry requests",
                                request.channel_id
                            ),
                        )
                        .await?;
                        return Ok(());
                    }
                    write_transfer_message(
                        &mut send,
                        &TransferResponse::ReadyFile {
                            size: registered.size,
                            content_hash: registered.content_hash.clone(),
                        },
                        "transfer response",
                    )
                    .await?;

                    let (_, mut credit_recv) = connection.accept_data_stream().await?;
                    let mut file = File::open(&registered.path)
                        .await
                        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                    if let Some(progress_tx) = &registered.progress_tx {
                        let _ = progress_tx.send(TransferRuntimeProgress {
                            channel_id: request.channel_id.clone(),
                            item_kind: TransferItemKind::File,
                            name: registered.display_name.clone(),
                            phase: TransferPhase::Downloading,
                            bytes_complete: 0,
                            bytes_total: Some(registered.size),
                        });
                    }
                    serve_file_with_credit(
                        &mut send,
                        &mut credit_recv,
                        &mut file,
                        registered.size,
                        |bytes_complete| {
                            if let Some(progress_tx) = &registered.progress_tx {
                                let _ = progress_tx.send(TransferRuntimeProgress {
                                    channel_id: request.channel_id.clone(),
                                    item_kind: TransferItemKind::File,
                                    name: registered.display_name.clone(),
                                    phase: TransferPhase::Downloading,
                                    bytes_complete,
                                    bytes_total: Some(registered.size),
                                });
                            }
                        },
                    )
                    .await?;
                    let _ = send.finish();
                    let _ = recv.read_to_end(1024).await;
                    Ok(())
                }
                .await;
                match file_send_result {
                    Ok(()) => {}
                    Err(TransportError::DirectFileSend(message))
                        if message.contains("closed stream") => {}
                    Err(error) => return Err(error),
                }
            }
            RegisteredTransfer::Directory(registered) => {
                if let Some(entry_path) = request.entry_path {
                    let (_, credit_recv) = connection.accept_data_stream().await?;
                    serve_directory_entry_request(
                        registered,
                        request.channel_id.clone(),
                        entry_path,
                        send,
                        recv,
                        credit_recv,
                    )
                    .await?;
                    return Ok(());
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
                    if let Some(progress_tx) = &registered.progress_tx {
                        let _ = progress_tx.send(TransferRuntimeProgress {
                            channel_id: request.channel_id.clone(),
                            item_kind: TransferItemKind::Folder,
                            name: registered.prepared.display_name.clone(),
                            phase: TransferPhase::Downloading,
                            bytes_complete: 0,
                            bytes_total: Some(registered.prepared.total_size),
                        });
                    }
                    send.finish()
                        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
                    let expected_file_requests = registered
                        .prepared
                        .entries
                        .iter()
                        .filter(|entry| entry.entry_kind == PreparedDirectoryEntryKind::File)
                        .count();
                    let mut join_set = tokio::task::JoinSet::new();
                    for _ in 0..expected_file_requests {
                        let (entry_send, mut entry_recv) = connection.accept_data_stream().await?;
                        let request: Option<TransferRequest> =
                            read_transfer_message(&mut entry_recv, "transfer request").await?;
                        let Some(request) = request else {
                            return Err(TransportError::DirectFileSend(
                                "directory transfer connection closed before all file requests arrived"
                                    .into(),
                            ));
                        };
                        let Some(entry_path) = request.entry_path else {
                            return Err(TransportError::DirectFileSend(format!(
                                "directory transfer channel {} expected entry request, got manifest",
                                request.channel_id
                            )));
                        };
                        let (_, credit_recv) = connection.accept_data_stream().await?;
                        let registered = registered.clone();
                        join_set.spawn(async move {
                            serve_directory_entry_request(
                                registered,
                                request.channel_id,
                                entry_path,
                                entry_send,
                                entry_recv,
                                credit_recv,
                            )
                            .await
                        });
                    }
                    while let Some(result) = join_set.join_next().await {
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => return Err(error),
                            Err(error) => {
                                return Err(TransportError::DirectFileSend(format!(
                                    "directory transfer task failed: {error}"
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
    entry_kind: PreparedDirectoryEntryKind,
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
    let mut saw_children = false;
    for entry in std::fs::read_dir(current)
        .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
    {
        saw_children = true;
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
            entry_kind: PreparedDirectoryEntryKind::File,
            size,
        });
    }
    if !saw_children && current != root {
        let relative_path = current
            .strip_prefix(root)
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
            .to_path_buf();
        entries.push(CollectionEntry {
            relative_path,
            entry_kind: PreparedDirectoryEntryKind::Directory,
            size: 0,
        });
    }
    Ok(())
}

fn collection_entry_name(relative_path: &Path) -> Result<String, TransportError> {
    let relative_path = validated_collection_relative_path(relative_path)?;
    Ok(relative_path.to_string_lossy().replace('\\', "/"))
}

async fn request_transfer_connection(
    transport: &IrohTransport,
    provider: &PeerTicket,
    request: TransferRequest,
) -> Result<(IrohConnection, SendStream, RecvStream), TransportError> {
    let connection = transport.connect_transfer(provider).await?;
    transport.enforce_connection_policy(&connection).await?;
    let (mut send, recv) = connection.open_data_stream().await?;
    write_transfer_message(&mut send, &request, "transfer request").await?;
    Ok((connection, send, recv))
}

async fn read_transfer_response_required(
    recv: &mut RecvStream,
) -> Result<TransferResponse, TransportError> {
    read_transfer_message(recv, "transfer response")
        .await?
        .ok_or_else(|| {
            TransportError::DirectFileReceive(
                "peer closed transfer connection before responding".into(),
            )
        })
}

fn expect_ready_file_response(
    response: TransferResponse,
    expected_size: Option<u64>,
    channel_id: &str,
) -> Result<u64, TransportError> {
    match response {
        TransferResponse::ReadyFile { size, .. } => {
            if let Some(expected_size) = expected_size {
                if size != expected_size {
                    return Err(TransportError::DirectFileReceive(format!(
                        "transfer size mismatch for {channel_id}: expected {expected_size}, provider advertised {size}"
                    )));
                }
            }
            Ok(size)
        }
        TransferResponse::ReadyDirectory { .. } => Err(TransportError::DirectFileReceive(format!(
            "transfer channel {channel_id} is a directory, not a file"
        ))),
        TransferResponse::FinalizedFile { .. } => Err(TransportError::DirectFileReceive(format!(
            "transfer channel {channel_id} sent file finalization before file metadata"
        ))),
        TransferResponse::Error { message } => Err(TransportError::DirectFileReceive(message)),
    }
}

fn expect_ready_directory_response(
    response: TransferResponse,
    channel_id: &str,
) -> Result<(u64, Vec<PreparedDirectoryEntry>), TransportError> {
    match response {
        TransferResponse::ReadyDirectory {
            total_size,
            entries,
        } => Ok((total_size, entries)),
        TransferResponse::Error { message } => Err(TransportError::DirectFileReceive(message)),
        TransferResponse::ReadyFile { .. } => Err(TransportError::DirectFileReceive(format!(
            "transfer channel {channel_id} is a file, not a directory"
        ))),
        TransferResponse::FinalizedFile { .. } => Err(TransportError::DirectFileReceive(format!(
            "transfer channel {channel_id} sent file finalization where directory metadata was expected"
        ))),
    }
}

async fn receive_file_to_temp(
    credit_send: &mut SendStream,
    recv: &mut RecvStream,
    expected_size: u64,
    target: &Path,
    mut on_bytes_complete: impl FnMut(u64),
) -> Result<String, TransportError> {
    let mut file = File::create(target)
        .await
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes_complete = 0_u64;
    let mut credit_pending = 0_u64;
    let mut credit_stream_open = true;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    write_transfer_credit(credit_send, TRANSFER_WINDOW_BYTES, "receiver credit").await?;
    loop {
        let read = match tokio::io::AsyncReadExt::read(recv, &mut buffer).await {
            Ok(read) => read,
            Err(_error) if bytes_complete >= expected_size => 0,
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
        if credit_stream_open
            && credit_pending >= TRANSFER_CREDIT_GRANT_BYTES
            && bytes_complete < expected_size
        {
            if write_transfer_credit(credit_send, credit_pending, "receiver credit")
                .await
                .is_ok()
            {
                credit_pending = 0;
            } else {
                credit_stream_open = false;
            }
        }
        on_bytes_complete(bytes_complete);
    }
    file.flush()
        .await
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;

    if bytes_complete != expected_size {
        return Err(TransportError::DirectFileReceive(format!(
            "transfer size mismatch: expected {expected_size} bytes, received {bytes_complete}"
        )));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn receive_directory_entries(
    connection: IrohConnection,
    channel_id: &str,
    temp_root: &Path,
    entries: &[PreparedDirectoryEntry],
    total_size: u64,
    worker_count: usize,
    on_progress: &mut impl FnMut(LocalTransferProgress),
) -> Result<(), TransportError> {
    let mut total_complete = 0_u64;
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let mut join_set = tokio::task::JoinSet::new();
    let stream_open_lock = Arc::new(Mutex::new(()));
    let file_entries = entries
        .iter()
        .filter(|entry| entry.entry_kind == PreparedDirectoryEntryKind::File)
        .cloned()
        .collect::<Vec<_>>();
    for entry in entries
        .iter()
        .filter(|entry| entry.entry_kind == PreparedDirectoryEntryKind::Directory)
    {
        let relative_path = validated_collection_relative_path(&entry.relative_path)
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
        tokio::fs::create_dir_all(temp_root.join(relative_path))
            .await
            .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    }
    let mut entry_index = 0_usize;
    let mut in_flight = 0_usize;
    let worker_count = worker_count.max(1);

    on_progress(LocalTransferProgress {
        phase: TransferPhase::Downloading,
        bytes_complete: 0,
        bytes_total: Some(total_size),
    });

    while entry_index < file_entries.len() && in_flight < worker_count {
        spawn_directory_entry_receive(
            &mut join_set,
            connection.clone(),
            channel_id.to_string(),
            temp_root.to_path_buf(),
            file_entries[entry_index].clone(),
            progress_tx.clone(),
            stream_open_lock.clone(),
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
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        join_set.abort_all();
                        return Err(error);
                    }
                    Err(error) => {
                        join_set.abort_all();
                        return Err(TransportError::DirectFileReceive(format!(
                            "directory transfer task failed: {error}"
                        )));
                    }
                }

                if entry_index < file_entries.len() {
                    spawn_directory_entry_receive(
                        &mut join_set,
                        connection.clone(),
                        channel_id.to_string(),
                        temp_root.to_path_buf(),
                        file_entries[entry_index].clone(),
                        progress_tx.clone(),
                        stream_open_lock.clone(),
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

    Ok(())
}

async fn write_transfer_error_response(
    send: &mut SendStream,
    message: String,
) -> Result<(), TransportError> {
    write_transfer_message(
        send,
        &TransferResponse::Error { message },
        "transfer response",
    )
    .await
}

fn lookup_directory_entry(
    registered: &RegisteredDirectory,
    channel_id: &str,
    entry_path: &str,
) -> Result<(PreparedDirectoryEntry, PathBuf), TransportError> {
    let entry = registered
        .prepared
        .entries
        .iter()
        .find(|entry| entry.relative_path == entry_path)
        .filter(|entry| entry.entry_kind == PreparedDirectoryEntryKind::File)
        .cloned()
        .ok_or_else(|| {
            TransportError::DirectFileSend(format!(
                "directory transfer channel {} has no entry {}",
                channel_id, entry_path
            ))
        })?;
    let relative_path = validated_collection_relative_path(&entry.relative_path)
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    Ok((entry, registered.root.join(relative_path)))
}

async fn serve_directory_entry_request(
    registered: RegisteredDirectory,
    channel_id: String,
    entry_path: String,
    mut send: SendStream,
    mut recv: RecvStream,
    mut credit_recv: RecvStream,
) -> Result<(), TransportError> {
    let (entry, source) = match lookup_directory_entry(&registered, &channel_id, &entry_path) {
        Ok(value) => value,
        Err(TransportError::DirectFileSend(message)) => {
            write_transfer_error_response(&mut send, message).await?;
            send.finish()
                .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    write_transfer_message(
        &mut send,
        &TransferResponse::ReadyFile {
            size: entry.size,
            content_hash: None,
        },
        "transfer response",
    )
    .await?;
    let mut file = File::open(&source)
        .await
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    let mut last_entry_bytes = 0_u64;
    let content_hash = serve_file_with_credit_and_hash(
        &mut send,
        &mut credit_recv,
        &mut file,
        entry.size,
        |entry_bytes_complete| {
            let delta = entry_bytes_complete.saturating_sub(last_entry_bytes);
            last_entry_bytes = entry_bytes_complete;
            if let Some(progress_tx) = &registered.progress_tx {
                let bytes_complete = registered
                    .aggregate_progress
                    .fetch_add(delta, std::sync::atomic::Ordering::Relaxed)
                    .saturating_add(delta);
                let _ = progress_tx.send(TransferRuntimeProgress {
                    channel_id: channel_id.clone(),
                    item_kind: TransferItemKind::Folder,
                    name: registered.prepared.display_name.clone(),
                    phase: TransferPhase::Downloading,
                    bytes_complete,
                    bytes_total: Some(registered.prepared.total_size),
                });
            }
        },
    )
    .await?;
    write_transfer_message(
        &mut send,
        &TransferResponse::FinalizedFile { content_hash },
        "transfer response",
    )
    .await?;
    send.finish()
        .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
    let _ = recv.read_to_end(1024).await;
    Ok(())
}

async fn receive_directory_entry(
    connection: IrohConnection,
    channel_id: String,
    temp_root: PathBuf,
    entry: PreparedDirectoryEntry,
    progress_tx: mpsc::UnboundedSender<u64>,
    stream_open_lock: Arc<Mutex<()>>,
) -> Result<(), TransportError> {
    let _stream_open_guard = stream_open_lock.lock().await;
    let (mut send, mut recv) = connection.open_data_stream().await?;
    let (mut credit_send, _) = connection.open_data_stream().await?;
    write_transfer_message(
        &mut send,
        &TransferRequest::entry(channel_id, entry.relative_path.clone()),
        "transfer request",
    )
    .await?;
    drop(_stream_open_guard);

    let size = expect_ready_file_response(
        read_transfer_response_required(&mut recv).await?,
        Some(entry.size),
        &entry.relative_path,
    )?;
    if size != entry.size {
        return Err(TransportError::DirectFileReceive(format!(
            "directory entry {} size mismatch: expected {}, provider advertised {}",
            entry.relative_path, entry.size, size
        )));
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
    write_transfer_credit(&mut credit_send, TRANSFER_WINDOW_BYTES, "receiver credit").await?;

    let mut hasher = blake3::Hasher::new();
    let mut bytes_complete = 0_u64;
    let mut credit_pending = 0_u64;
    let mut credit_stream_open = true;
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
        if credit_stream_open
            && credit_pending >= TRANSFER_CREDIT_GRANT_BYTES
            && bytes_complete < entry.size
        {
            if write_transfer_credit(&mut credit_send, credit_pending, "receiver credit")
                .await
                .is_ok()
            {
                credit_pending = 0;
            } else {
                credit_stream_open = false;
            }
        }
    }
    file.flush()
        .await
        .map_err(|error| TransportError::DirectFileReceive(error.to_string()))?;
    let finalized_hash = match read_transfer_response_required(&mut recv).await? {
        TransferResponse::FinalizedFile { content_hash } => content_hash,
        TransferResponse::Error { message } => {
            return Err(TransportError::DirectFileReceive(message));
        }
        other => {
            return Err(TransportError::DirectFileReceive(format!(
                "directory entry {} sent unexpected trailer: {other:?}",
                entry.relative_path
            )));
        }
    };
    let actual_hash = hasher.finalize().to_hex().to_string();
    if actual_hash != finalized_hash {
        return Err(TransportError::DirectFileReceive(format!(
            "transfer hash mismatch for {}: expected {}, received {}",
            entry.relative_path, finalized_hash, actual_hash
        )));
    }
    let _ = credit_send.finish();
    let _ = send.finish();
    Ok(())
}

fn spawn_directory_entry_receive(
    join_set: &mut tokio::task::JoinSet<Result<(), TransportError>>,
    connection: IrohConnection,
    channel_id: String,
    temp_root: PathBuf,
    entry: PreparedDirectoryEntry,
    progress_tx: mpsc::UnboundedSender<u64>,
    stream_open_lock: Arc<Mutex<()>>,
) {
    join_set.spawn(async move {
        receive_directory_entry(
            connection,
            channel_id,
            temp_root,
            entry,
            progress_tx,
            stream_open_lock,
        )
        .await
    });
}

async fn serve_file_with_credit(
    send: &mut SendStream,
    credit_recv: &mut RecvStream,
    file: &mut File,
    expected_size: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<(), TransportError> {
    let mut available_credit = read_transfer_credit(credit_recv, "receiver credit")
        .await
        .map_err(|error| {
            TransportError::DirectFileSend(format!(
                "failed reading initial receiver credit at 0/{expected_size} bytes: {error}"
            ))
        })?;
    let mut bytes_complete = 0_u64;
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    while bytes_complete < expected_size {
        if available_credit == 0 {
            available_credit = available_credit
                .saturating_add(
                    read_transfer_credit(credit_recv, "receiver credit")
                        .await
                        .map_err(|error| {
                            TransportError::DirectFileSend(format!(
                                "failed reading receiver credit at {bytes_complete}/{expected_size} bytes: {error}"
                            ))
                        })?,
                );
            continue;
        }
        let chunk_len = available_credit
            .min(buffer.len() as u64)
            .min(expected_size.saturating_sub(bytes_complete)) as usize;
        let read = file
            .read(&mut buffer[..chunk_len])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        if read == 0 {
            return Err(TransportError::DirectFileSend(format!(
                "transfer source shorter than advertised: expected {expected_size} bytes, sent {bytes_complete}"
            )));
        }
        let next_bytes_complete = bytes_complete.saturating_add(read as u64);
        match send.write_all(&buffer[..read]).await {
            Ok(()) => {
                available_credit = available_credit.saturating_sub(read as u64);
                bytes_complete = next_bytes_complete;
                on_progress(bytes_complete);
            }
            Err(error)
                if next_bytes_complete == expected_size
                    && error.to_string().contains("closed stream") =>
            {
                bytes_complete = next_bytes_complete;
                on_progress(bytes_complete);
                break;
            }
            Err(error) => {
                return Err(TransportError::DirectFileSend(format!(
                "failed writing file chunk at {next_bytes_complete}/{expected_size} bytes: {error}"
            )))
            }
        }
    }
    Ok(())
}

async fn serve_file_with_credit_and_hash(
    send: &mut SendStream,
    credit_recv: &mut RecvStream,
    file: &mut File,
    expected_size: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<String, TransportError> {
    let mut available_credit = read_transfer_credit(credit_recv, "receiver credit").await?;
    let mut bytes_complete = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; TRANSFER_CHUNK_BYTES];
    while bytes_complete < expected_size {
        if available_credit == 0 {
            available_credit = available_credit
                .saturating_add(read_transfer_credit(credit_recv, "receiver credit").await?);
            continue;
        }
        let chunk_len = available_credit
            .min(buffer.len() as u64)
            .min(expected_size.saturating_sub(bytes_complete)) as usize;
        let read = file
            .read(&mut buffer[..chunk_len])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        if read == 0 {
            return Err(TransportError::DirectFileSend(format!(
                "transfer source shorter than advertised: expected {expected_size} bytes, sent {bytes_complete}"
            )));
        }
        send.write_all(&buffer[..read])
            .await
            .map_err(|error| TransportError::DirectFileSend(error.to_string()))?;
        hasher.update(&buffer[..read]);
        available_credit = available_credit.saturating_sub(read as u64);
        bytes_complete = bytes_complete.saturating_add(read as u64);
        on_progress(bytes_complete);
    }
    Ok(hasher.finalize().to_hex().to_string())
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
    let path = path.to_path_buf();
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<u64>();
    let task = tokio::task::spawn_blocking(move || hash_file_blocking(path, progress_tx));
    tokio::pin!(task);

    let result = loop {
        tokio::select! {
            maybe_progress = progress_rx.recv() => {
                match maybe_progress {
                    Some(bytes_complete) => on_progress(bytes_complete.min(size)),
                    None => {
                        let result = (&mut task).await;
                        break result;
                    }
                }
            }
            result = &mut task => break result,
        }
    };

    while let Ok(bytes_complete) = progress_rx.try_recv() {
        on_progress(bytes_complete.min(size));
    }

    result.map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?
}

fn hash_file_blocking(
    path: PathBuf,
    progress_tx: mpsc::UnboundedSender<u64>,
) -> Result<String, TransportError> {
    let mut file = std::fs::File::open(&path)
        .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    let mut bytes_complete = 0_u64;
    let mut last_reported = 0_u64;
    let mut buffer = vec![0_u8; PREP_HASH_BUFFER_BYTES];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| TransportError::DirectFilePrepare(error.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes_complete = bytes_complete.saturating_add(read as u64);
        if bytes_complete.saturating_sub(last_reported) >= PREP_HASH_PROGRESS_GRANULARITY_BYTES {
            let _ = progress_tx.send(bytes_complete);
            last_reported = bytes_complete;
        }
    }

    if bytes_complete != last_reported {
        let _ = progress_tx.send(bytes_complete);
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
    send.write_all(&bytes).await.map_err(|error| {
        TransportError::TransferProtocol(format!("failed to write {label}: {error}"))
    })?;
    send.flush().await.map_err(|error| {
        TransportError::TransferProtocol(format!("failed to flush {label}: {error}"))
    })?;
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
    recv.read_exact(&mut frame[4..]).await.map_err(|error| {
        TransportError::TransferProtocol(format!("failed to read {label} payload: {error}"))
    })?;
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

async fn read_transfer_credit(recv: &mut RecvStream, label: &str) -> Result<u64, TransportError> {
    let message: Option<TransferReceiverMessage> = read_transfer_message(recv, label).await?;
    let message = message.ok_or_else(|| {
        TransportError::TransferProtocol(format!(
            "peer closed {label} stream before granting credit"
        ))
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
