//! Room-native machine protocol.
//!
//! This module defines the canonical command and event types for Skyffla's
//! machine-facing room API. It is intentionally kept separate from transport
//! and runtime orchestration so wrappers can rely on one clean, documented
//! contract.
//!
//! The important semantic split is:
//!
//! - host-authoritative traffic: room welcome, member snapshots, membership deltas
//! - direct member traffic: routed chat and channel payload events
//!
//! The machine API exposes both as typed events; wrappers do not need to know
//! which transport path carried a given message.

use std::fmt;

use serde::{Deserialize, Serialize};

pub const MACHINE_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomProtocolError {
    EmptyIdentifier { kind: &'static str },
    EmptyPath { kind: &'static str },
    EmptyChatMessage,
    EmptyChannelData,
    EmptyMemberSnapshot,
    MissingBlobRef,
    UnexpectedBlobRef,
}

impl fmt::Display for RoomProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentifier { kind } => write!(f, "{kind} must not be empty"),
            Self::EmptyPath { kind } => write!(f, "{kind} must not be empty"),
            Self::EmptyChatMessage => write!(f, "chat message must not be empty"),
            Self::EmptyChannelData => write!(f, "channel data must not be empty"),
            Self::EmptyMemberSnapshot => write!(f, "member snapshot must not be empty"),
            Self::MissingBlobRef => write!(f, "file channels require blob metadata"),
            Self::UnexpectedBlobRef => write!(f, "blob metadata is only allowed for file channels"),
        }
    }
}

impl std::error::Error for RoomProtocolError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct RoomId(String);

impl RoomId {
    pub fn new(value: impl Into<String>) -> Result<Self, RoomProtocolError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier { kind: "room_id" });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct MemberId(String);

impl MemberId {
    pub fn new(value: impl Into<String>) -> Result<Self, RoomProtocolError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct ChannelId(String);

impl ChannelId {
    pub fn new(value: impl Into<String>) -> Result<Self, RoomProtocolError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Route {
    All,
    Member { member_id: MemberId },
}

impl Route {
    pub fn validate(&self) -> Result<(), RoomProtocolError> {
        match self {
            Self::All => Ok(()),
            Self::Member { member_id } => {
                if member_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Machine,
    File,
    Clipboard,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlobFormat {
    Blob,
    Collection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferItemKind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Preparing,
    Downloading,
    Exporting,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRef {
    pub hash: String,
    pub format: BlobFormat,
}

impl BlobRef {
    pub fn validate(&self) -> Result<(), RoomProtocolError> {
        if self.hash.trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier { kind: "blob_hash" });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub member_id: MemberId,
    pub name: String,
    pub fingerprint: Option<String>,
}

impl Member {
    pub fn validate(&self) -> Result<(), RoomProtocolError> {
        if self.member_id.as_str().trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
        }
        if self.name.trim().is_empty() {
            return Err(RoomProtocolError::EmptyIdentifier {
                kind: "member_name",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MachineCommand {
    LeaveRoom,
    SendChat {
        to: Route,
        text: String,
    },
    SendFile {
        channel_id: ChannelId,
        to: Route,
        path: String,
        name: Option<String>,
        mime: Option<String>,
    },
    OpenChannel {
        channel_id: ChannelId,
        kind: ChannelKind,
        to: Route,
        name: Option<String>,
        size: Option<u64>,
        mime: Option<String>,
        blob: Option<BlobRef>,
    },
    AcceptChannel {
        channel_id: ChannelId,
    },
    RejectChannel {
        channel_id: ChannelId,
        reason: Option<String>,
    },
    SendChannelData {
        channel_id: ChannelId,
        body: String,
    },
    CloseChannel {
        channel_id: ChannelId,
        reason: Option<String>,
    },
    ExportChannelFile {
        channel_id: ChannelId,
        path: String,
    },
}

impl MachineCommand {
    pub fn validate(&self) -> Result<(), RoomProtocolError> {
        match self {
            Self::LeaveRoom => Ok(()),
            Self::SendChat { to, text } => {
                to.validate()?;
                if text.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyChatMessage);
                }
                Ok(())
            }
            Self::SendFile {
                channel_id,
                to,
                path,
                ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                to.validate()?;
                if path.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyPath { kind: "path" });
                }
                Ok(())
            }
            Self::OpenChannel {
                channel_id,
                kind,
                to,
                blob,
                ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                to.validate()?;
                validate_channel_blob(kind, blob.as_ref())
            }
            Self::AcceptChannel { channel_id }
            | Self::RejectChannel { channel_id, .. }
            | Self::CloseChannel { channel_id, .. } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                Ok(())
            }
            Self::ExportChannelFile { channel_id, path } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if path.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyPath { kind: "path" });
                }
                Ok(())
            }
            Self::SendChannelData { channel_id, body } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if body.is_empty() {
                    return Err(RoomProtocolError::EmptyChannelData);
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MachineEvent {
    RoomWelcome {
        protocol_version: u16,
        room_id: RoomId,
        self_member: MemberId,
        host_member: MemberId,
    },
    MemberSnapshot {
        members: Vec<Member>,
    },
    MemberJoined {
        member: Member,
    },
    MemberLeft {
        member_id: MemberId,
        reason: Option<String>,
    },
    RoomClosed {
        reason: String,
    },
    Chat {
        from: MemberId,
        from_name: String,
        to: Route,
        text: String,
    },
    ChannelOpened {
        channel_id: ChannelId,
        kind: ChannelKind,
        from: MemberId,
        from_name: String,
        to: Route,
        name: Option<String>,
        size: Option<u64>,
        mime: Option<String>,
        blob: Option<BlobRef>,
    },
    ChannelAccepted {
        channel_id: ChannelId,
        member_id: MemberId,
        member_name: String,
    },
    ChannelRejected {
        channel_id: ChannelId,
        member_id: MemberId,
        member_name: String,
        reason: Option<String>,
    },
    ChannelData {
        channel_id: ChannelId,
        from: MemberId,
        from_name: String,
        body: String,
    },
    ChannelClosed {
        channel_id: ChannelId,
        member_id: MemberId,
        member_name: String,
        reason: Option<String>,
    },
    ChannelFileReady {
        channel_id: ChannelId,
        blob: BlobRef,
    },
    ChannelFileExported {
        channel_id: ChannelId,
        path: String,
        size: u64,
    },
    TransferProgress {
        channel_id: ChannelId,
        item_kind: TransferItemKind,
        name: String,
        phase: TransferPhase,
        bytes_complete: u64,
        bytes_total: Option<u64>,
    },
    Error {
        code: String,
        message: String,
        channel_id: Option<ChannelId>,
    },
}

impl MachineEvent {
    pub fn validate(&self) -> Result<(), RoomProtocolError> {
        match self {
            Self::RoomWelcome {
                protocol_version,
                room_id,
                self_member,
                host_member,
            } => {
                if *protocol_version == 0 {
                    return Err(RoomProtocolError::EmptyIdentifier {
                        kind: "protocol_version",
                    });
                }
                if room_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "room_id" });
                }
                if self_member.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                if host_member.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                Ok(())
            }
            Self::MemberSnapshot { members } => {
                if members.is_empty() {
                    return Err(RoomProtocolError::EmptyMemberSnapshot);
                }
                for member in members {
                    member.validate()?;
                }
                Ok(())
            }
            Self::MemberJoined { member } => member.validate(),
            Self::MemberLeft { member_id, .. } => {
                if member_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                Ok(())
            }
            Self::RoomClosed { reason } => {
                if reason.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "reason" });
                }
                Ok(())
            }
            Self::Chat {
                from,
                from_name,
                to,
                text,
            } => {
                if from.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                if from_name.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier {
                        kind: "member_name",
                    });
                }
                to.validate()?;
                if text.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyChatMessage);
                }
                Ok(())
            }
            Self::ChannelOpened {
                channel_id,
                kind,
                from,
                from_name,
                to,
                blob,
                ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if from.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                if from_name.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier {
                        kind: "member_name",
                    });
                }
                to.validate()?;
                validate_channel_blob(kind, blob.as_ref())
            }
            Self::ChannelAccepted {
                channel_id,
                member_id,
                member_name,
            }
            | Self::ChannelRejected {
                channel_id,
                member_id,
                member_name,
                ..
            }
            | Self::ChannelClosed {
                channel_id,
                member_id,
                member_name,
                ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if member_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                if member_name.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier {
                        kind: "member_name",
                    });
                }
                Ok(())
            }
            Self::ChannelData {
                channel_id,
                from,
                from_name,
                body,
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if from.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "member_id" });
                }
                if from_name.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier {
                        kind: "member_name",
                    });
                }
                if body.is_empty() {
                    return Err(RoomProtocolError::EmptyChannelData);
                }
                Ok(())
            }
            Self::ChannelFileReady { channel_id, blob } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                blob.validate()
            }
            Self::ChannelFileExported {
                channel_id, path, ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if path.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyPath { kind: "path" });
                }
                Ok(())
            }
            Self::TransferProgress {
                channel_id,
                name,
                ..
            } => {
                if channel_id.as_str().trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "channel_id" });
                }
                if name.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "name" });
                }
                Ok(())
            }
            Self::Error { code, message, .. } => {
                if code.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "code" });
                }
                if message.trim().is_empty() {
                    return Err(RoomProtocolError::EmptyIdentifier { kind: "message" });
                }
                Ok(())
            }
        }
    }
}

fn validate_channel_blob(
    kind: &ChannelKind,
    blob: Option<&BlobRef>,
) -> Result<(), RoomProtocolError> {
    match kind {
        ChannelKind::File => match blob {
            Some(blob) => blob.validate(),
            None => Err(RoomProtocolError::MissingBlobRef),
        },
        ChannelKind::Machine | ChannelKind::Clipboard => match blob {
            Some(_) => Err(RoomProtocolError::UnexpectedBlobRef),
            None => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_member(id: &str, name: &str) -> Member {
        Member {
            member_id: MemberId::new(id).expect("valid member id"),
            name: name.into(),
            fingerprint: Some(format!("fp-{id}")),
        }
    }

    #[test]
    fn send_chat_command_round_trips_via_json() {
        let command = MachineCommand::SendChat {
            to: Route::All,
            text: "hello room".into(),
        };

        let encoded = serde_json::to_string(&command).expect("serialize command");
        let decoded: MachineCommand = serde_json::from_str(&encoded).expect("deserialize command");

        assert_eq!(decoded, command);
        decoded.validate().expect("command should validate");
    }

    #[test]
    fn room_welcome_event_round_trips_via_json() {
        let event = MachineEvent::RoomWelcome {
            protocol_version: MACHINE_PROTOCOL_VERSION,
            room_id: RoomId::new("warehouse").expect("valid room id"),
            self_member: MemberId::new("m1").expect("valid member id"),
            host_member: MemberId::new("m1").expect("valid member id"),
        };

        let encoded = serde_json::to_string(&event).expect("serialize event");
        let decoded: MachineEvent = serde_json::from_str(&encoded).expect("deserialize event");

        assert_eq!(decoded, event);
        decoded.validate().expect("event should validate");
    }

    #[test]
    fn member_snapshot_event_validates_members() {
        let event = MachineEvent::MemberSnapshot {
            members: vec![sample_member("m1", "alpha"), sample_member("m2", "beta")],
        };

        assert_eq!(event.validate(), Ok(()));
    }

    #[test]
    fn command_validation_rejects_empty_chat() {
        let command = MachineCommand::SendChat {
            to: Route::All,
            text: "  ".into(),
        };

        assert_eq!(command.validate(), Err(RoomProtocolError::EmptyChatMessage));
    }

    #[test]
    fn command_validation_rejects_empty_channel_data() {
        let command = MachineCommand::SendChannelData {
            channel_id: ChannelId::new("c1").expect("valid channel id"),
            body: String::new(),
        };

        assert_eq!(command.validate(), Err(RoomProtocolError::EmptyChannelData));
    }

    #[test]
    fn event_validation_rejects_empty_member_snapshot() {
        let event = MachineEvent::MemberSnapshot { members: vec![] };

        assert_eq!(
            event.validate(),
            Err(RoomProtocolError::EmptyMemberSnapshot)
        );
    }

    #[test]
    fn documented_chat_event_shape_round_trips() {
        let json = r#"{
            "type":"chat",
            "from":"m2",
            "from_name":"beta",
            "to":{"type":"all"},
            "text":"hello"
        }"#;

        let event: MachineEvent = serde_json::from_str(json).expect("chat event should parse");
        assert_eq!(
            event,
            MachineEvent::Chat {
                from: MemberId::new("m2").expect("valid member id"),
                from_name: "beta".into(),
                to: Route::All,
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn documented_open_channel_command_shape_round_trips() {
        let json = r#"{
            "type":"open_channel",
            "channel_id":"c7",
            "kind":"machine",
            "to":{"type":"member","member_id":"m2"},
            "name":"agent-link"
        }"#;

        let command: MachineCommand =
            serde_json::from_str(json).expect("open_channel command should parse");

        assert_eq!(
            command,
            MachineCommand::OpenChannel {
                channel_id: ChannelId::new("c7").expect("valid channel id"),
                kind: ChannelKind::Machine,
                to: Route::Member {
                    member_id: MemberId::new("m2").expect("valid member id"),
                },
                name: Some("agent-link".into()),
                size: None,
                mime: None,
                blob: None,
            }
        );
    }

    #[test]
    fn documented_send_file_command_shape_round_trips() {
        let json = r#"{
            "type":"send_file",
            "channel_id":"c9",
            "to":{"type":"member","member_id":"m2"},
            "path":"./report.txt",
            "name":"report.txt",
            "mime":"text/plain"
        }"#;

        let command: MachineCommand =
            serde_json::from_str(json).expect("send_file command should parse");

        assert_eq!(
            command,
            MachineCommand::SendFile {
                channel_id: ChannelId::new("c9").expect("valid channel id"),
                to: Route::Member {
                    member_id: MemberId::new("m2").expect("valid member id"),
                },
                path: "./report.txt".into(),
                name: Some("report.txt".into()),
                mime: Some("text/plain".into()),
            }
        );
        command.validate().expect("send_file should validate");
    }

    #[test]
    fn documented_accept_channel_command_shape_round_trips() {
        let json = r#"{
            "type":"accept_channel",
            "channel_id":"c7"
        }"#;

        let command: MachineCommand =
            serde_json::from_str(json).expect("accept_channel command should parse");

        assert_eq!(
            command,
            MachineCommand::AcceptChannel {
                channel_id: ChannelId::new("c7").expect("valid channel id"),
            }
        );
        command.validate().expect("accept_channel should validate");
    }

    #[test]
    fn documented_file_channel_command_with_blob_round_trips() {
        let json = r#"{
            "type":"open_channel",
            "channel_id":"c8",
            "kind":"file",
            "to":{"type":"member","member_id":"m2"},
            "name":"report.pdf",
            "size":1234,
            "mime":"application/pdf",
            "blob":{"hash":"abc123","format":"blob"}
        }"#;

        let command: MachineCommand =
            serde_json::from_str(json).expect("file open_channel command should parse");

        assert_eq!(
            command,
            MachineCommand::OpenChannel {
                channel_id: ChannelId::new("c8").expect("valid channel id"),
                kind: ChannelKind::File,
                to: Route::Member {
                    member_id: MemberId::new("m2").expect("valid member id"),
                },
                name: Some("report.pdf".into()),
                size: Some(1234),
                mime: Some("application/pdf".into()),
                blob: Some(BlobRef {
                    hash: "abc123".into(),
                    format: BlobFormat::Blob,
                }),
            }
        );
        command.validate().expect("file channel should validate");
    }

    #[test]
    fn documented_channel_accepted_event_shape_round_trips() {
        let json = r#"{
            "type":"channel_accepted",
            "channel_id":"c7",
            "member_id":"m1",
            "member_name":"alpha"
        }"#;

        let event: MachineEvent =
            serde_json::from_str(json).expect("channel_accepted event should parse");

        assert_eq!(
            event,
            MachineEvent::ChannelAccepted {
                channel_id: ChannelId::new("c7").expect("valid channel id"),
                member_id: MemberId::new("m1").expect("valid member id"),
                member_name: "alpha".into(),
            }
        );
        event.validate().expect("channel_accepted should validate");
    }

    #[test]
    fn file_channel_requires_blob_ref() {
        let command = MachineCommand::OpenChannel {
            channel_id: ChannelId::new("c1").expect("valid channel id"),
            kind: ChannelKind::File,
            to: Route::All,
            name: Some("report.pdf".into()),
            size: None,
            mime: None,
            blob: None,
        };

        assert_eq!(command.validate(), Err(RoomProtocolError::MissingBlobRef));
    }

    #[test]
    fn file_ready_event_with_blob_round_trips() {
        let json = r#"{
            "type":"channel_file_ready",
            "channel_id":"c1",
            "blob":{"hash":"abc123","format":"blob"}
        }"#;

        let event: MachineEvent =
            serde_json::from_str(json).expect("channel_file_ready should parse");

        assert_eq!(
            event,
            MachineEvent::ChannelFileReady {
                channel_id: ChannelId::new("c1").expect("valid channel id"),
                blob: BlobRef {
                    hash: "abc123".into(),
                    format: BlobFormat::Blob,
                },
            }
        );
        event
            .validate()
            .expect("channel_file_ready should validate");
    }

    #[test]
    fn documented_error_event_shape_round_trips() {
        let json = r#"{
            "type":"error",
            "code":"command_failed",
            "message":"channel c7 is closed",
            "channel_id":"c7"
        }"#;

        let event: MachineEvent = serde_json::from_str(json).expect("error event should parse");

        assert_eq!(
            event,
            MachineEvent::Error {
                code: "command_failed".into(),
                message: "channel c7 is closed".into(),
                channel_id: Some(ChannelId::new("c7").expect("valid channel id")),
            }
        );
        event.validate().expect("error event should validate");
    }

    #[test]
    fn transfer_progress_event_round_trips() {
        let json = r#"{
            "type":"transfer_progress",
            "channel_id":"c9",
            "item_kind":"folder",
            "name":"photos",
            "phase":"downloading",
            "bytes_complete":32,
            "bytes_total":64
        }"#;

        let event: MachineEvent =
            serde_json::from_str(json).expect("transfer_progress should parse");

        assert_eq!(
            event,
            MachineEvent::TransferProgress {
                channel_id: ChannelId::new("c9").expect("valid channel id"),
                item_kind: TransferItemKind::Folder,
                name: "photos".into(),
                phase: TransferPhase::Downloading,
                bytes_complete: 32,
                bytes_total: Some(64),
            }
        );
        event.validate().expect("transfer_progress should validate");
    }
}
