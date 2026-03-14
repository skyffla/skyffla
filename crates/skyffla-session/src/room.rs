use std::collections::BTreeMap;
use std::fmt;

use skyffla_protocol::room::{
    MachineEvent, Member, MemberId, RoomId, RoomProtocolError, Route, MACHINE_PROTOCOL_VERSION,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedEvent {
    pub recipient: MemberId,
    pub event: MachineEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinDispatch {
    pub member: Member,
    pub to_joiner: Vec<MachineEvent>,
    pub to_existing_members: Vec<RoutedEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveDispatch {
    pub departed_member: MemberId,
    pub to_remaining_members: Vec<RoutedEvent>,
}

#[derive(Debug)]
pub struct RoomEngine {
    room_id: RoomId,
    host_member: MemberId,
    members: BTreeMap<MemberId, Member>,
    next_member_index: u64,
}

impl RoomEngine {
    pub fn new(
        room_id: RoomId,
        host_name: impl Into<String>,
        host_fingerprint: Option<String>,
    ) -> Result<Self, RoomEngineError> {
        let host_member = Member {
            member_id: MemberId::new("m1")?,
            name: host_name.into(),
            fingerprint: host_fingerprint,
        };
        host_member.validate()?;

        let mut members = BTreeMap::new();
        members.insert(host_member.member_id.clone(), host_member.clone());

        Ok(Self {
            room_id,
            host_member: host_member.member_id,
            members,
            next_member_index: 2,
        })
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn host_member(&self) -> &MemberId {
        &self.host_member
    }

    pub fn members(&self) -> &BTreeMap<MemberId, Member> {
        &self.members
    }

    pub fn join(
        &mut self,
        name: impl Into<String>,
        fingerprint: Option<String>,
    ) -> Result<JoinDispatch, RoomEngineError> {
        let member_id = MemberId::new(format!("m{}", self.next_member_index))?;
        let member = Member {
            member_id: member_id.clone(),
            name: name.into(),
            fingerprint,
        };
        member.validate()?;

        let existing_member_ids = self.members.keys().cloned().collect::<Vec<_>>();
        self.members.insert(member_id.clone(), member.clone());
        self.next_member_index += 1;

        let to_joiner = vec![
            MachineEvent::RoomWelcome {
                protocol_version: MACHINE_PROTOCOL_VERSION,
                room_id: self.room_id.clone(),
                self_member: member_id.clone(),
                host_member: self.host_member.clone(),
            },
            MachineEvent::MemberSnapshot {
                members: self.members.values().cloned().collect(),
            },
        ];

        let joined_event = MachineEvent::MemberJoined {
            member: member.clone(),
        };
        let to_existing_members = existing_member_ids
            .into_iter()
            .map(|recipient| RoutedEvent {
                recipient,
                event: joined_event.clone(),
            })
            .collect();

        Ok(JoinDispatch {
            member,
            to_joiner,
            to_existing_members,
        })
    }

    pub fn leave(
        &mut self,
        member_id: &MemberId,
        reason: Option<String>,
    ) -> Result<LeaveDispatch, RoomEngineError> {
        if member_id == &self.host_member {
            return Err(RoomEngineError::HostCannotLeaveRoom);
        }

        if self.members.remove(member_id).is_none() {
            return Err(RoomEngineError::UnknownMember {
                member_id: member_id.clone(),
            });
        }

        let event = MachineEvent::MemberLeft {
            member_id: member_id.clone(),
            reason,
        };
        let to_remaining_members = self
            .members
            .keys()
            .cloned()
            .map(|recipient| RoutedEvent {
                recipient,
                event: event.clone(),
            })
            .collect();

        Ok(LeaveDispatch {
            departed_member: member_id.clone(),
            to_remaining_members,
        })
    }

    pub fn send_chat(
        &self,
        from: &MemberId,
        to: Route,
        text: impl Into<String>,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        self.require_member(from)?;

        let text = text.into();
        let event = MachineEvent::Chat {
            from: from.clone(),
            to: to.clone(),
            text,
        };
        event.validate()?;

        match to {
            Route::All => Ok(self
                .members
                .keys()
                .filter(|member_id| *member_id != from)
                .cloned()
                .map(|recipient| RoutedEvent {
                    recipient,
                    event: event.clone(),
                })
                .collect()),
            Route::Member { member_id } => {
                self.require_member(&member_id)?;
                if &member_id == from {
                    return Ok(vec![]);
                }
                Ok(vec![RoutedEvent {
                    recipient: member_id,
                    event,
                }])
            }
        }
    }

    fn require_member(&self, member_id: &MemberId) -> Result<(), RoomEngineError> {
        if self.members.contains_key(member_id) {
            Ok(())
        } else {
            Err(RoomEngineError::UnknownMember {
                member_id: member_id.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEngineError {
    Protocol(RoomProtocolError),
    UnknownMember { member_id: MemberId },
    HostCannotLeaveRoom,
}

impl fmt::Display for RoomEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{error}"),
            Self::UnknownMember { member_id } => {
                write!(f, "unknown room member {}", member_id.as_str())
            }
            Self::HostCannotLeaveRoom => write!(f, "host cannot leave its own room"),
        }
    }
}

impl std::error::Error for RoomEngineError {}

impl From<RoomProtocolError> for RoomEngineError {
    fn from(value: RoomProtocolError) -> Self {
        Self::Protocol(value)
    }
}

#[cfg(test)]
mod tests {
    use skyffla_protocol::room::MachineEvent;

    use super::*;

    fn member_id(value: &str) -> MemberId {
        MemberId::new(value).expect("valid member id")
    }

    fn room_id(value: &str) -> RoomId {
        RoomId::new(value).expect("valid room id")
    }

    #[test]
    fn host_owns_one_room_with_initial_member() {
        let room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room created");

        assert_eq!(room.room_id().as_str(), "warehouse");
        assert_eq!(room.host_member().as_str(), "m1");
        assert_eq!(room.members().len(), 1);
        assert!(room.members().contains_key(&member_id("m1")));
    }

    #[test]
    fn join_assigns_stable_member_ids_and_emits_snapshot() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");

        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        assert_eq!(beta.member.member_id.as_str(), "m2");
        assert_eq!(gamma.member.member_id.as_str(), "m3");
        assert!(matches!(
            beta.to_joiner.first(),
            Some(MachineEvent::RoomWelcome { .. })
        ));
        assert!(matches!(
            beta.to_joiner.get(1),
            Some(MachineEvent::MemberSnapshot { .. })
        ));
        assert_eq!(beta.to_existing_members.len(), 1);
        assert_eq!(gamma.to_existing_members.len(), 2);
    }

    #[test]
    fn leave_broadcasts_to_remaining_members() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        let dispatch = room
            .leave(&beta.member.member_id, Some("bye".into()))
            .expect("leave succeeds");

        assert_eq!(dispatch.departed_member, beta.member.member_id);
        assert_eq!(dispatch.to_remaining_members.len(), 2);
        assert!(dispatch
            .to_remaining_members
            .iter()
            .any(|event| event.recipient == member_id("m1")));
        assert!(dispatch
            .to_remaining_members
            .iter()
            .any(|event| event.recipient == gamma.member.member_id));
    }

    #[test]
    fn broadcast_chat_fans_out_to_current_members_except_sender() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        let events = room
            .send_chat(&beta.member.member_id, Route::All, "hello room")
            .expect("chat succeeds");

        assert_eq!(events.len(), 2);
        assert!(events.iter().any(|event| event.recipient == member_id("m1")));
        assert!(events
            .iter()
            .any(|event| event.recipient == gamma.member.member_id));
        assert!(events
            .iter()
            .all(|event| matches!(event.event, MachineEvent::Chat { .. })));
    }

    #[test]
    fn direct_chat_reaches_only_the_target_member() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        let events = room
            .send_chat(
                &beta.member.member_id,
                Route::Member {
                    member_id: gamma.member.member_id.clone(),
                },
                "secret",
            )
            .expect("chat succeeds");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].recipient, gamma.member.member_id);
    }

    #[test]
    fn stale_members_are_not_included_in_broadcasts() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        room.leave(&gamma.member.member_id, None).expect("gamma leaves");

        let events = room
            .send_chat(&beta.member.member_id, Route::All, "after leave")
            .expect("chat succeeds");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].recipient, member_id("m1"));
    }
}
