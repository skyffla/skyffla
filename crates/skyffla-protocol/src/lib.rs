mod framing;
pub mod room;
pub mod room_link;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use framing::{
    decode_frame, encode_frame, read_frame, write_frame, FrameError, MAX_FRAME_SIZE,
};

pub const WIRE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);
pub const FILE_TRANSFER_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(4, 0);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub const fn is_compatible_with(self, other: Self) -> bool {
        self.major == other.major
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TransportCapability {
    NativeDirect,
    WebsocketRelay,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Stdio,
    Machine,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub chat: bool,
    pub file: bool,
    pub folder: bool,
    pub clipboard: bool,
    pub stdio: bool,
    pub transport: Vec<TransportCapability>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            chat: true,
            file: true,
            folder: true,
            clipboard: true,
            stdio: true,
            transport: vec![TransportCapability::NativeDirect],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Hello,
    HelloAck,
    ChatMessage,
    Offer,
    Accept,
    Reject,
    Progress,
    Complete,
    Cancel,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub session_id: String,
    pub message_id: String,
    pub message_type: MessageType,
    pub payload: ControlMessage,
}

impl Envelope {
    pub fn new(
        session_id: impl Into<String>,
        message_id: impl Into<String>,
        payload: ControlMessage,
    ) -> Self {
        let message_type = payload.message_type();
        Self {
            version: WIRE_PROTOCOL_VERSION,
            session_id: session_id.into(),
            message_id: message_id.into(),
            message_type,
            payload,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        let actual = self.payload.message_type();
        if self.message_type != actual {
            return Err(ProtocolError::MessageTypeMismatch {
                declared: self.message_type.clone(),
                actual,
            });
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ControlMessage {
    Hello(Hello),
    HelloAck(HelloAck),
    ChatMessage(ChatMessage),
    Offer(Offer),
    Accept(Accept),
    Reject(Reject),
    Progress(Progress),
    Complete(Complete),
    Cancel(Cancel),
    Error(ErrorMessage),
}

impl ControlMessage {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Hello(_) => MessageType::Hello,
            Self::HelloAck(_) => MessageType::HelloAck,
            Self::ChatMessage(_) => MessageType::ChatMessage,
            Self::Offer(_) => MessageType::Offer,
            Self::Accept(_) => MessageType::Accept,
            Self::Reject(_) => MessageType::Reject,
            Self::Progress(_) => MessageType::Progress,
            Self::Complete(_) => MessageType::Complete,
            Self::Cancel(_) => MessageType::Cancel,
            Self::Error(_) => MessageType::Error,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: ProtocolVersion,
    #[serde(default)]
    pub file_transfer_version: Option<ProtocolVersion>,
    pub session_id: String,
    pub peer_name: String,
    pub peer_fingerprint: Option<String>,
    pub peer_ticket: Option<String>,
    pub capabilities: Capabilities,
    pub transport_capabilities: Vec<TransportCapability>,
    pub session_mode: SessionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloAck {
    pub protocol_version: ProtocolVersion,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferKind {
    File,
    FolderArchive,
    Clipboard,
    Stdio,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Tar,
    TarZstd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Offer {
    pub transfer_id: String,
    pub kind: TransferKind,
    pub name: String,
    pub size: Option<u64>,
    pub mime: Option<String>,
    pub item_count: Option<u64>,
    pub compression: Option<Compression>,
    pub path_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Accept {
    pub transfer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reject {
    pub transfer_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Progress {
    pub transfer_id: String,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Digest {
    pub algorithm: String,
    pub value_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Complete {
    pub transfer_id: String,
    pub digest: Option<Digest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cancel {
    pub transfer_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorMessage {
    pub code: String,
    pub message: String,
    pub transfer_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataStreamHeader {
    pub transfer_id: String,
    pub kind: TransferKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    MessageTypeMismatch {
        declared: MessageType,
        actual: MessageType,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessageTypeMismatch { declared, actual } => {
                write!(
                    f,
                    "message type mismatch: declared {declared:?}, actual {actual:?}"
                )
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn sample_envelope() -> Envelope {
        Envelope::new(
            "demo-session",
            "m1",
            ControlMessage::Offer(Offer {
                transfer_id: "t1".into(),
                kind: TransferKind::File,
                name: "notes.txt".into(),
                size: Some(12),
                mime: Some("text/plain".into()),
                item_count: None,
                compression: None,
                path_hint: Some("notes.txt".into()),
            }),
        )
    }

    #[test]
    fn envelope_validation_accepts_matching_types() {
        let envelope = sample_envelope();
        assert_eq!(envelope.validate(), Ok(()));
    }

    #[test]
    fn envelope_validation_rejects_mismatched_types() {
        let mut envelope = sample_envelope();
        envelope.message_type = MessageType::ChatMessage;

        assert_eq!(
            envelope.validate(),
            Err(ProtocolError::MessageTypeMismatch {
                declared: MessageType::ChatMessage,
                actual: MessageType::Offer,
            })
        );
    }

    #[test]
    fn frame_round_trip_via_buffer() {
        let envelope = sample_envelope();
        let encoded = encode_frame(&envelope).expect("encoding should succeed");
        let decoded: Envelope = decode_frame(&encoded).expect("decoding should succeed");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn frame_round_trip_via_io() {
        let envelope = sample_envelope();
        let mut cursor = Cursor::new(Vec::new());
        write_frame(&mut cursor, &envelope).expect("writing should succeed");

        cursor.set_position(0);
        let decoded: Envelope = read_frame(&mut cursor).expect("reading should succeed");

        assert_eq!(decoded, envelope);
    }

    #[test]
    fn decode_rejects_length_prefix_mismatch() {
        let envelope = sample_envelope();
        let mut encoded = encode_frame(&envelope).expect("encoding should succeed");
        encoded[3] = encoded[3].saturating_add(1);

        let result = decode_frame::<Envelope>(&encoded);
        assert!(matches!(
            result,
            Err(FrameError::InvalidLengthPrefix { .. })
        ));
    }
}
