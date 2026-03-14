use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::ProtocolHandler;
use iroh::{Endpoint, EndpointAddr, RelayMode, TransportAddr};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::{BlobFormat as IrohBlobFormat, BlobsProtocol, Hash, HashAndFormat};
use skyffla_protocol::room::{BlobFormat, BlobRef};
use skyffla_protocol::TransportCapability;
use tokio::sync::{mpsc, Mutex};

pub const SKYFFLA_ALPN: &[u8] = b"skyffla/native/1";

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
    accept_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    blob_store: MemStore,
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
pub struct ImportedPath {
    pub blob: BlobRef,
    pub size: u64,
}

impl IrohTransport {
    pub async fn bind() -> Result<Self, TransportError> {
        Self::bind_with_options(TransportOptions::default()).await
    }

    pub async fn bind_with_options(options: TransportOptions) -> Result<Self, TransportError> {
        let mut builder =
            Endpoint::builder().alpns(vec![SKYFFLA_ALPN.to_vec(), iroh_blobs::ALPN.to_vec()]);
        if options.local_only {
            builder = builder.relay_mode(RelayMode::Disabled);
        }
        let endpoint = builder.bind().await.map_err(TransportError::EndpointBind)?;
        let blob_store = MemStore::new();
        let blobs_protocol = BlobsProtocol::new(&blob_store, None);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let accept_task = tokio::spawn(run_accept_loop(
            endpoint.clone(),
            incoming_tx,
            blobs_protocol,
        ));

        Ok(Self {
            inner: Arc::new(TransportInner {
                endpoint,
                options,
                incoming: Mutex::new(incoming_rx),
                accept_task: Mutex::new(Some(accept_task)),
                blob_store,
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

    pub async fn import_blob_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<BlobRef, TransportError> {
        let mut tag = self
            .inner
            .blob_store
            .blobs()
            .add_path(path)
            .temp_tag()
            .await
            .map_err(|error| TransportError::BlobImport(error.to_string()))?;
        tag.leak();
        Ok(blob_ref_from_hash_and_format(tag.hash_and_format()))
    }

    pub async fn import_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ImportedPath, TransportError> {
        let path = path.as_ref();
        if path.is_file() {
            let blob = self.import_blob_path(path).await?;
            let size = std::fs::metadata(path)
                .map_err(|error| TransportError::BlobImport(error.to_string()))?
                .len();
            return Ok(ImportedPath { blob, size });
        }
        if path.is_dir() {
            return self.import_collection_path(path).await;
        }
        Err(TransportError::BlobImport(format!(
            "path {} is not a file or directory",
            path.display()
        )))
    }

    pub async fn fetch_blob(
        &self,
        provider: &PeerTicket,
        blob: &BlobRef,
    ) -> Result<(), TransportError> {
        let endpoint_addr =
            filter_endpoint_addr(provider.to_endpoint_addr()?, self.inner.options.local_only);
        if self.inner.options.local_only && endpoint_addr.addrs.is_empty() {
            return Err(TransportError::NoLocalAddresses);
        }

        let connection = self
            .inner
            .endpoint
            .connect(endpoint_addr, iroh_blobs::ALPN)
            .await
            .map_err(TransportError::ConnectBlob)?;
        let wrapped = IrohConnection {
            connection: connection.clone(),
        };
        self.enforce_connection_policy(&wrapped).await?;

        let hash_and_format = hash_and_format_from_blob_ref(blob)?;
        self.inner
            .blob_store
            .remote()
            .fetch(connection, hash_and_format)
            .await
            .map_err(|error| TransportError::BlobFetch(error.to_string()))?;
        let mut tag = self
            .inner
            .blob_store
            .tags()
            .temp_tag(hash_and_format)
            .await
            .map_err(|error| TransportError::BlobTag(error.to_string()))?;
        tag.leak();
        Ok(())
    }

    pub async fn export_blob(
        &self,
        blob: &BlobRef,
        target: impl AsRef<Path>,
    ) -> Result<u64, TransportError> {
        let hash = hash_from_blob_ref(blob)?;
        self.inner
            .blob_store
            .blobs()
            .export(hash, target)
            .await
            .map_err(|error| TransportError::BlobExport(error.to_string()))
    }

    pub async fn export_path(
        &self,
        blob: &BlobRef,
        target: impl AsRef<Path>,
    ) -> Result<u64, TransportError> {
        match blob.format {
            BlobFormat::Blob => self.export_blob(blob, target).await,
            BlobFormat::Collection => self.export_collection(blob, target.as_ref()).await,
        }
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
        let _ = self.inner.blob_store.shutdown().await;
    }
}

impl IrohTransport {
    async fn import_collection_path(&self, path: &Path) -> Result<ImportedPath, TransportError> {
        let entries = collect_collection_entries(path)?;
        let mut total_size = 0_u64;
        let mut tags = Vec::with_capacity(entries.len());
        let mut items = Vec::with_capacity(entries.len());

        for entry in entries {
            total_size = total_size.saturating_add(entry.size);
            let tag = self
                .inner
                .blob_store
                .blobs()
                .add_path(&entry.absolute_path)
                .temp_tag()
                .await
                .map_err(|error| TransportError::BlobImport(error.to_string()))?;
            let name = collection_entry_name(&entry.relative_path)?;
            items.push((name, tag.hash()));
            tags.push(tag);
        }

        let collection = Collection::from_iter(items);
        let mut root = collection
            .store(self.inner.blob_store.as_ref())
            .await
            .map_err(|error| TransportError::BlobImport(error.to_string()))?;
        for tag in &mut tags {
            tag.leak();
        }
        root.leak();

        Ok(ImportedPath {
            blob: blob_ref_from_hash_and_format(root.hash_and_format()),
            size: total_size,
        })
    }

    async fn export_collection(&self, blob: &BlobRef, target: &Path) -> Result<u64, TransportError> {
        std::fs::create_dir_all(target)
            .map_err(|error| TransportError::BlobExport(error.to_string()))?;
        let hash = hash_from_blob_ref(blob)?;
        let collection = Collection::load(hash, self.inner.blob_store.as_ref())
            .await
            .map_err(|error| TransportError::BlobExport(error.to_string()))?;

        let mut total_size = 0_u64;
        for (name, hash) in collection {
            let relative_path = validated_collection_relative_path(&name)?;
            let destination = target.join(&relative_path);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| TransportError::BlobExport(error.to_string()))?;
            }
            let size = self
                .inner
                .blob_store
                .blobs()
                .export(hash, &destination)
                .await
                .map_err(|error| TransportError::BlobExport(error.to_string()))?;
            total_size = total_size.saturating_add(size);
        }
        Ok(total_size)
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
    for entry in std::fs::read_dir(current).map_err(|error| TransportError::BlobImport(error.to_string()))? {
        let entry = entry.map_err(|error| TransportError::BlobImport(error.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| TransportError::BlobImport(error.to_string()))?;

        if file_type.is_dir() {
            collect_collection_entries_recursive(root, &path, entries)?;
            continue;
        }
        if !(file_type.is_file() || file_type.is_symlink()) {
            continue;
        }

        let relative_path = path
            .strip_prefix(root)
            .map_err(|error| TransportError::BlobImport(error.to_string()))?
            .to_path_buf();
        let size = std::fs::metadata(&path)
            .map_err(|error| TransportError::BlobImport(error.to_string()))?
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

fn validated_collection_relative_path(path: impl AsRef<Path>) -> Result<PathBuf, TransportError> {
    let path = path.as_ref();
    if path.is_absolute() {
        return Err(TransportError::BlobExport(format!(
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
                return Err(TransportError::BlobExport(format!(
                    "collection entry {} contains invalid path components",
                    path.display()
                )));
            }
        }
    }

    if clean.as_os_str().is_empty() {
        return Err(TransportError::BlobExport(
            "collection entry path must not be empty".into(),
        ));
    }

    Ok(clean)
}

async fn run_accept_loop(
    endpoint: Endpoint,
    incoming_tx: mpsc::UnboundedSender<IrohConnection>,
    blobs_protocol: BlobsProtocol,
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
            iroh_blobs::ALPN => {
                let protocol = blobs_protocol.clone();
                tokio::spawn(async move {
                    let _ = protocol.accept(connection).await;
                });
            }
            _ => {}
        }
    }
}

fn blob_ref_from_hash_and_format(hash_and_format: HashAndFormat) -> BlobRef {
    BlobRef {
        hash: hash_and_format.hash.to_hex(),
        format: blob_format_from_iroh(hash_and_format.format),
    }
}

fn hash_from_blob_ref(blob: &BlobRef) -> Result<Hash, TransportError> {
    Hash::from_str(&blob.hash).map_err(|error| TransportError::BlobHashDecode(error.to_string()))
}

fn hash_and_format_from_blob_ref(blob: &BlobRef) -> Result<HashAndFormat, TransportError> {
    Ok(HashAndFormat::new(
        hash_from_blob_ref(blob)?,
        blob_format_to_iroh(&blob.format),
    ))
}

fn blob_format_from_iroh(format: IrohBlobFormat) -> BlobFormat {
    match format {
        IrohBlobFormat::Raw => BlobFormat::Blob,
        IrohBlobFormat::HashSeq => BlobFormat::Collection,
    }
}

fn blob_format_to_iroh(format: &BlobFormat) -> IrohBlobFormat {
    match format {
        BlobFormat::Blob => IrohBlobFormat::Raw,
        BlobFormat::Collection => IrohBlobFormat::HashSeq,
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
    ConnectBlob(iroh::endpoint::ConnectError),
    OpenBi(iroh::endpoint::ConnectionError),
    AcceptBi(iroh::endpoint::ConnectionError),
    TicketEncode(serde_json::Error),
    TicketDecode(serde_json::Error),
    BlobImport(String),
    BlobFetch(String),
    BlobExport(String),
    BlobTag(String),
    BlobHashDecode(String),
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
            Self::ConnectBlob(error) => write!(f, "failed to connect for blob transfer: {error}"),
            Self::OpenBi(error) => write!(f, "failed to open bidirectional stream: {error}"),
            Self::AcceptBi(error) => write!(f, "failed to accept bidirectional stream: {error}"),
            Self::TicketEncode(error) => write!(f, "failed to encode peer ticket: {error}"),
            Self::TicketDecode(error) => write!(f, "failed to decode peer ticket: {error}"),
            Self::BlobImport(error) => write!(f, "failed to import blob: {error}"),
            Self::BlobFetch(error) => write!(f, "failed to fetch blob: {error}"),
            Self::BlobExport(error) => write!(f, "failed to export blob: {error}"),
            Self::BlobTag(error) => write!(f, "failed to pin blob: {error}"),
            Self::BlobHashDecode(error) => write!(f, "failed to decode blob hash: {error}"),
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
            Self::ConnectBlob(error) => Some(error),
            Self::OpenBi(error) => Some(error),
            Self::AcceptBi(error) => Some(error),
            Self::TicketEncode(error) => Some(error),
            Self::TicketDecode(error) => Some(error),
            Self::EndpointClosed
            | Self::BlobImport(_)
            | Self::BlobFetch(_)
            | Self::BlobExport(_)
            | Self::BlobTag(_)
            | Self::BlobHashDecode(_)
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
    async fn import_and_export_blob_round_trip() {
        let Some(transport) = bind_or_skip().await else {
            return;
        };
        let source = unique_temp_path("blob-source");
        let target = unique_temp_path("blob-target");
        std::fs::write(&source, b"blob data").expect("source file should be writable");

        let blob = transport
            .import_blob_path(&source)
            .await
            .expect("blob import should succeed");
        let size = transport
            .export_blob(&blob, &target)
            .await
            .expect("blob export should succeed");

        assert_eq!(size, 9);
        assert_eq!(
            std::fs::read(&target).expect("target should exist"),
            b"blob data"
        );

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&target);
        transport.close().await;
    }

    #[tokio::test]
    async fn fetch_blob_downloads_from_peer() {
        let Some(host) = bind_or_skip().await else {
            return;
        };
        let Some(joiner) = bind_or_skip().await else {
            return;
        };
        let source = unique_temp_path("fetch-source");
        let target = unique_temp_path("fetch-target");
        std::fs::write(&source, b"hello blob peer").expect("source file should be writable");

        let blob = host
            .import_blob_path(&source)
            .await
            .expect("host should import blob");
        let host_ticket = host.local_ticket().expect("host ticket should encode");

        joiner
            .fetch_blob(&host_ticket, &blob)
            .await
            .expect("joiner should fetch blob");
        let size = joiner
            .export_blob(&blob, &target)
            .await
            .expect("joiner should export fetched blob");

        assert_eq!(size, 15);
        assert_eq!(
            std::fs::read(&target).expect("target should exist"),
            b"hello blob peer"
        );

        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&target);
        host.close().await;
        joiner.close().await;
    }

    #[tokio::test]
    async fn import_and_export_collection_round_trip() {
        let Some(transport) = bind_or_skip().await else {
            return;
        };
        let source_dir = unique_temp_path("collection-source");
        let nested_dir = source_dir.join("nested");
        let target_dir = unique_temp_path("collection-target");
        std::fs::create_dir_all(&nested_dir).expect("nested source dir should be writable");
        std::fs::write(source_dir.join("a.txt"), b"alpha").expect("a.txt should be writable");
        std::fs::write(nested_dir.join("b.txt"), b"beta").expect("b.txt should be writable");

        let imported = transport
            .import_path(&source_dir)
            .await
            .expect("collection import should succeed");
        assert_eq!(imported.blob.format, BlobFormat::Collection);
        assert_eq!(imported.size, 9);

        let exported_size = transport
            .export_path(&imported.blob, &target_dir)
            .await
            .expect("collection export should succeed");
        assert_eq!(exported_size, 9);
        assert_eq!(
            std::fs::read(target_dir.join("a.txt")).expect("exported a.txt should exist"),
            b"alpha"
        );
        assert_eq!(
            std::fs::read(target_dir.join("nested").join("b.txt"))
                .expect("exported nested b.txt should exist"),
            b"beta"
        );

        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
        transport.close().await;
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
