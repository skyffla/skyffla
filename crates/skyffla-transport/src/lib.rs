use std::fmt;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, TransportAddr};
use skyffla_protocol::TransportCapability;

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

#[derive(Debug, Clone)]
pub struct IrohTransport {
    endpoint: Endpoint,
}

impl IrohTransport {
    pub async fn bind() -> Result<Self, TransportError> {
        let endpoint = Endpoint::builder()
            .alpns(vec![SKYFFLA_ALPN.to_vec()])
            .bind()
            .await
            .map_err(TransportError::EndpointBind)?;
        Ok(Self { endpoint })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn local_ticket(&self) -> Result<PeerTicket, TransportError> {
        let addr = self.endpoint.addr();
        PeerTicket::from_endpoint_addr(&addr)
    }

    pub async fn accept_connection(&self) -> Result<IrohConnection, TransportError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(TransportError::EndpointClosed)?;
        let connection = incoming.await.map_err(TransportError::Accept)?;
        Ok(IrohConnection { connection })
    }

    pub async fn connect(&self, ticket: &PeerTicket) -> Result<IrohConnection, TransportError> {
        let endpoint_addr = ticket.to_endpoint_addr()?;
        let connection = self
            .endpoint
            .connect(endpoint_addr, SKYFFLA_ALPN)
            .await
            .map_err(TransportError::Connect)?;
        Ok(IrohConnection { connection })
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

        let remote_info_ip = self.endpoint.remote_info(remote_id).await.and_then(|info| {
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

    pub async fn close(self) {
        self.endpoint.close().await;
    }
}

impl PeerTransport for IrohTransport {
    type Connection = IrohConnection;
    type Error = TransportError;

    fn transport_capabilities(&self) -> Vec<TransportCapability> {
        vec![TransportCapability::NativeDirect]
    }
}

#[derive(Debug)]
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
    Accept(iroh::endpoint::ConnectingError),
    Connect(iroh::endpoint::ConnectError),
    OpenBi(iroh::endpoint::ConnectionError),
    AcceptBi(iroh::endpoint::ConnectionError),
    TicketEncode(serde_json::Error),
    TicketDecode(serde_json::Error),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointBind(error) => write!(f, "failed to bind iroh endpoint: {error}"),
            Self::EndpointClosed => write!(f, "iroh endpoint closed before accepting a connection"),
            Self::Accept(error) => write!(f, "failed to accept iroh connection: {error}"),
            Self::Connect(error) => write!(f, "failed to connect via iroh: {error}"),
            Self::OpenBi(error) => write!(f, "failed to open bidirectional stream: {error}"),
            Self::AcceptBi(error) => write!(f, "failed to accept bidirectional stream: {error}"),
            Self::TicketEncode(error) => write!(f, "failed to encode peer ticket: {error}"),
            Self::TicketDecode(error) => write!(f, "failed to decode peer ticket: {error}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EndpointBind(error) => Some(error),
            Self::Accept(error) => Some(error),
            Self::Connect(error) => Some(error),
            Self::OpenBi(error) => Some(error),
            Self::AcceptBi(error) => Some(error),
            Self::TicketEncode(error) => Some(error),
            Self::TicketDecode(error) => Some(error),
            Self::EndpointClosed => None,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
