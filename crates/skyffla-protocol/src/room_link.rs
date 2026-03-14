//! Internal room link protocol.
//!
//! These messages travel between room members on Skyffla-controlled links.
//! They are distinct from the public `room` machine API:
//!
//! - `room` is the wrapper-facing stdin/stdout contract.
//! - `room_link` is the internal peer protocol used to coordinate authority
//!   links and peer introductions between Skyffla processes.
//!
//! Keeping this separate lets runtimes evolve without pushing transport or
//! membership details into wrapper-facing machine events.

use serde::{Deserialize, Serialize};

use crate::room::{MachineCommand, MachineEvent, Member};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoomLinkMessage {
    MachineCommand {
        command: MachineCommand,
    },
    MachineEvent {
        event: MachineEvent,
    },
    PeerIntroduction {
        member: Member,
        ticket: String,
    },
}

impl RoomLinkMessage {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::MachineCommand { command } => command.validate().map_err(|err| err.to_string()),
            Self::MachineEvent { event } => event.validate().map_err(|err| err.to_string()),
            Self::PeerIntroduction { member, ticket } => {
                member.validate().map_err(|err| err.to_string())?;
                if ticket.trim().is_empty() {
                    return Err("peer ticket must not be empty".into());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::room::{MemberId, Route};

    use super::*;

    #[test]
    fn machine_command_round_trips_via_json() {
        let message = RoomLinkMessage::MachineCommand {
            command: MachineCommand::SendChat {
                to: Route::All,
                text: "hello".into(),
            },
        };

        let encoded = serde_json::to_string(&message).expect("serialize");
        let decoded: RoomLinkMessage = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded, message);
        decoded.validate().expect("valid message");
    }

    #[test]
    fn peer_introduction_requires_non_empty_ticket() {
        let message = RoomLinkMessage::PeerIntroduction {
            member: Member {
                member_id: MemberId::new("m2").expect("member id"),
                name: "beta".into(),
                fingerprint: None,
            },
            ticket: "   ".into(),
        };

        assert_eq!(
            message.validate(),
            Err("peer ticket must not be empty".into())
        );
    }
}
