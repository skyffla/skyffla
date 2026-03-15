//! Internal room link protocols.
//!
//! These messages travel between Skyffla processes on links that are managed by
//! the room runtime. They are distinct from the public machine API:
//!
//! - `room` is the wrapper-facing stdin/stdout contract.
//! - `room_link` is the internal protocol used to coordinate authority links
//!   and peer links between Skyffla processes.
//!
//! Keeping these separate lets the CLI/runtime evolve without pushing transport
//! or membership details into wrapper-facing machine events.
//!
//! The split is intentional:
//!
//! - authority links carry setup, membership, and host-authoritative commands
//! - peer links carry peer introductions and direct member-delivered events

use serde::{Deserialize, Serialize};

use crate::room::{MachineCommand, MachineEvent, Member};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthorityLinkMessage {
    MachineCommand {
        command: MachineCommand,
    },
    MachineEvent {
        event: MachineEvent,
    },
    PeerConnect {
        member: Member,
        ticket: String,
        connect: bool,
    },
}

impl AuthorityLinkMessage {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::MachineCommand { command } => command.validate().map_err(|err| err.to_string()),
            Self::MachineEvent { event } => event.validate().map_err(|err| err.to_string()),
            Self::PeerConnect { member, ticket, .. } => {
                member.validate().map_err(|err| err.to_string())?;
                if ticket.trim().is_empty() {
                    return Err("peer ticket must not be empty".into());
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerLinkMessage {
    PeerHello { member: Member },
    MachineEvent { event: MachineEvent },
}

impl PeerLinkMessage {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::PeerHello { member } => member.validate().map_err(|err| err.to_string()),
            Self::MachineEvent { event } => event.validate().map_err(|err| err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::room::{MemberId, Route};

    use super::*;

    #[test]
    fn authority_command_round_trips_via_json() {
        let message = AuthorityLinkMessage::MachineCommand {
            command: MachineCommand::SendChat {
                to: Route::All,
                text: "hello".into(),
            },
        };

        let encoded = serde_json::to_string(&message).expect("serialize");
        let decoded: AuthorityLinkMessage = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded, message);
        decoded.validate().expect("valid authority message");
    }

    #[test]
    fn authority_peer_connect_requires_non_empty_ticket() {
        let message = AuthorityLinkMessage::PeerConnect {
            member: Member {
                member_id: MemberId::new("m2").expect("member id"),
                name: "beta".into(),
                fingerprint: None,
            },
            ticket: "   ".into(),
            connect: true,
        };

        assert_eq!(
            message.validate(),
            Err("peer ticket must not be empty".into())
        );
    }

    #[test]
    fn peer_hello_round_trips_via_json() {
        let message = PeerLinkMessage::PeerHello {
            member: Member {
                member_id: MemberId::new("m2").expect("member id"),
                name: "beta".into(),
                fingerprint: None,
            },
        };

        let encoded = serde_json::to_string(&message).expect("serialize");
        let decoded: PeerLinkMessage = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded, message);
        decoded.validate().expect("valid peer message");
    }
}
