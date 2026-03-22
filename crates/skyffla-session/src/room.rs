use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use skyffla_protocol::room::{
    ChannelId, ChannelKind, MachineEvent, Member, MemberId, RoomId, RoomProtocolError, Route,
    TransferOffer, MACHINE_PROTOCOL_VERSION,
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
    channels: BTreeMap<ChannelId, ChannelState>,
    next_member_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelState {
    opener: MemberId,
    kind: ChannelKind,
    participants: BTreeSet<MemberId>,
    name: Option<String>,
    size: Option<u64>,
    mime: Option<String>,
    transfer: Option<TransferOffer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomChannel {
    pub opener: MemberId,
    pub kind: ChannelKind,
    pub participants: BTreeSet<MemberId>,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub mime: Option<String>,
    pub transfer: Option<TransferOffer>,
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
            channels: BTreeMap::new(),
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

    pub fn member(&self, member_id: &MemberId) -> Option<&Member> {
        self.members.get(member_id)
    }

    pub fn channel(&self, channel_id: &ChannelId) -> Option<RoomChannel> {
        self.channels.get(channel_id).map(|channel| RoomChannel {
            opener: channel.opener.clone(),
            kind: channel.kind.clone(),
            participants: channel.participants.clone(),
            name: channel.name.clone(),
            size: channel.size,
            mime: channel.mime.clone(),
            transfer: channel.transfer.clone(),
        })
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
        self.prune_member_from_channels(member_id);

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
        let from_name = self.member_name(from)?;
        let event = MachineEvent::Chat {
            from: from.clone(),
            from_name,
            to: to.clone(),
            text,
        };
        event.validate()?;

        self.route_event(from, to, event)
    }

    pub fn open_channel(
        &mut self,
        from: &MemberId,
        channel_id: ChannelId,
        kind: ChannelKind,
        to: Route,
        name: Option<String>,
        size: Option<u64>,
        mime: Option<String>,
        transfer: Option<TransferOffer>,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        self.require_member(from)?;
        if self.channels.contains_key(&channel_id) {
            return Err(RoomEngineError::ChannelAlreadyOpen { channel_id });
        }

        let from_name = self.member_name(from)?;
        let participants = self.channel_participants(from, &to)?;
        let event = MachineEvent::ChannelOpened {
            channel_id: channel_id.clone(),
            kind: kind.clone(),
            from: from.clone(),
            from_name,
            to,
            name,
            size,
            mime,
            transfer,
        };
        event.validate()?;
        self.channels.insert(
            channel_id,
            ChannelState {
                opener: from.clone(),
                kind,
                participants: participants.clone(),
                name: event_name(&event),
                size: event_size(&event),
                mime: event_mime(&event),
                transfer: event_transfer(&event),
            },
        );

        Ok(participants
            .into_iter()
            .filter(|member_id| member_id != from)
            .map(|recipient| RoutedEvent {
                recipient,
                event: event.clone(),
            })
            .collect())
    }

    pub fn accept_channel(
        &self,
        member_id: &MemberId,
        channel_id: &ChannelId,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        let member_name = self.member_name(member_id)?;
        self.route_channel_event(
            member_id,
            channel_id,
            MachineEvent::ChannelAccepted {
                channel_id: channel_id.clone(),
                member_id: member_id.clone(),
                member_name,
            },
        )
    }

    pub fn update_channel_transfer(
        &mut self,
        member_id: &MemberId,
        channel_id: &ChannelId,
        size: Option<u64>,
        transfer: TransferOffer,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        self.require_member(member_id)?;
        let channel =
            self.channels
                .get_mut(channel_id)
                .ok_or_else(|| RoomEngineError::UnknownChannel {
                    channel_id: channel_id.clone(),
                })?;
        if channel.opener != *member_id {
            return Err(RoomEngineError::NotChannelOpener {
                member_id: member_id.clone(),
                channel_id: channel_id.clone(),
            });
        }
        if channel.kind != ChannelKind::File {
            return Err(RoomEngineError::ChannelTransferUpdateUnsupported {
                channel_id: channel_id.clone(),
                kind: channel.kind.clone(),
            });
        }

        let event = match transfer.item_kind {
            skyffla_protocol::room::TransferItemKind::File => {
                MachineEvent::ChannelTransferFinalized {
                    channel_id: channel_id.clone(),
                    size,
                    transfer,
                }
            }
            skyffla_protocol::room::TransferItemKind::Folder => {
                MachineEvent::ChannelTransferReady {
                    channel_id: channel_id.clone(),
                    size,
                    transfer,
                }
            }
        };
        event.validate()?;
        channel.size = event_size(&event);
        channel.transfer = event_transfer(&event);

        Ok(channel
            .participants
            .iter()
            .filter(|recipient| *recipient != member_id)
            .filter(|recipient| self.members.contains_key(*recipient))
            .cloned()
            .map(|recipient| RoutedEvent {
                recipient,
                event: event.clone(),
            })
            .collect())
    }

    pub fn reject_channel(
        &mut self,
        member_id: &MemberId,
        channel_id: &ChannelId,
        reason: Option<String>,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        let member_name = self.member_name(member_id)?;
        let routed = self.route_channel_event(
            member_id,
            channel_id,
            MachineEvent::ChannelRejected {
                channel_id: channel_id.clone(),
                member_id: member_id.clone(),
                member_name,
                reason,
            },
        )?;
        self.remove_channel_participant(channel_id, member_id);
        Ok(routed)
    }

    pub fn send_channel_data(
        &self,
        from: &MemberId,
        channel_id: &ChannelId,
        body: String,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        let from_name = self.member_name(from)?;
        self.route_channel_event(
            from,
            channel_id,
            MachineEvent::ChannelData {
                channel_id: channel_id.clone(),
                from: from.clone(),
                from_name,
                body,
            },
        )
    }

    pub fn close_channel(
        &mut self,
        member_id: &MemberId,
        channel_id: &ChannelId,
        reason: Option<String>,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        let member_name = self.member_name(member_id)?;
        let routed = self.route_channel_event(
            member_id,
            channel_id,
            MachineEvent::ChannelClosed {
                channel_id: channel_id.clone(),
                member_id: member_id.clone(),
                member_name,
                reason,
            },
        )?;
        self.remove_channel_participant(channel_id, member_id);
        Ok(routed)
    }

    pub fn route_event(
        &self,
        sender: &MemberId,
        to: Route,
        event: MachineEvent,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        self.require_member(sender)?;

        match to {
            Route::All => Ok(self
                .members
                .keys()
                .filter(|member_id| *member_id != sender)
                .cloned()
                .map(|recipient| RoutedEvent {
                    recipient,
                    event: event.clone(),
                })
                .collect()),
            Route::Member { member_id } => {
                self.require_member(&member_id)?;
                if &member_id == sender {
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

    pub fn member_name(&self, member_id: &MemberId) -> Result<String, RoomEngineError> {
        self.member(member_id)
            .map(|member| member.name.clone())
            .ok_or_else(|| RoomEngineError::UnknownMember {
                member_id: member_id.clone(),
            })
    }

    fn channel_participants(
        &self,
        sender: &MemberId,
        to: &Route,
    ) -> Result<BTreeSet<MemberId>, RoomEngineError> {
        self.require_member(sender)?;
        let mut participants = BTreeSet::from([sender.clone()]);
        match to {
            Route::All => {
                for member_id in self.members.keys() {
                    participants.insert(member_id.clone());
                }
            }
            Route::Member { member_id } => {
                self.require_member(member_id)?;
                participants.insert(member_id.clone());
            }
        }
        Ok(participants)
    }

    fn route_channel_event(
        &self,
        sender: &MemberId,
        channel_id: &ChannelId,
        event: MachineEvent,
    ) -> Result<Vec<RoutedEvent>, RoomEngineError> {
        self.require_member(sender)?;
        event.validate()?;
        let channel =
            self.channels
                .get(channel_id)
                .ok_or_else(|| RoomEngineError::UnknownChannel {
                    channel_id: channel_id.clone(),
                })?;
        if matches!(event, MachineEvent::ChannelData { .. }) && channel.kind == ChannelKind::File {
            return Err(RoomEngineError::ChannelDataUnsupported {
                channel_id: channel_id.clone(),
                kind: channel.kind.clone(),
            });
        }
        if !channel.participants.contains(sender) {
            return Err(RoomEngineError::MemberNotInChannel {
                member_id: sender.clone(),
                channel_id: channel_id.clone(),
            });
        }

        Ok(channel
            .participants
            .iter()
            .filter(|member_id| *member_id != sender)
            .filter(|member_id| self.members.contains_key(*member_id))
            .cloned()
            .map(|recipient| RoutedEvent {
                recipient,
                event: event.clone(),
            })
            .collect())
    }

    fn remove_channel_participant(&mut self, channel_id: &ChannelId, member_id: &MemberId) {
        let should_remove = if let Some(channel) = self.channels.get_mut(channel_id) {
            channel.participants.remove(member_id);
            channel.participants.len() < 2
        } else {
            false
        };
        if should_remove {
            self.channels.remove(channel_id);
        }
    }

    fn prune_member_from_channels(&mut self, member_id: &MemberId) {
        let empty_channels = self
            .channels
            .iter_mut()
            .filter_map(|(channel_id, channel)| {
                channel.participants.remove(member_id);
                (channel.participants.len() < 2).then(|| channel_id.clone())
            })
            .collect::<Vec<_>>();
        for channel_id in empty_channels {
            self.channels.remove(&channel_id);
        }
    }
}

fn event_name(event: &MachineEvent) -> Option<String> {
    match event {
        MachineEvent::ChannelOpened { name, .. } => name.clone(),
        _ => None,
    }
}

fn event_size(event: &MachineEvent) -> Option<u64> {
    match event {
        MachineEvent::ChannelOpened { size, .. }
        | MachineEvent::ChannelTransferReady { size, .. }
        | MachineEvent::ChannelTransferFinalized { size, .. } => *size,
        _ => None,
    }
}

fn event_mime(event: &MachineEvent) -> Option<String> {
    match event {
        MachineEvent::ChannelOpened { mime, .. } => mime.clone(),
        _ => None,
    }
}

fn event_transfer(event: &MachineEvent) -> Option<TransferOffer> {
    match event {
        MachineEvent::ChannelOpened { transfer, .. } => transfer.clone(),
        MachineEvent::ChannelTransferReady { transfer, .. }
        | MachineEvent::ChannelTransferFinalized { transfer, .. } => Some(transfer.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEngineError {
    Protocol(RoomProtocolError),
    UnknownMember {
        member_id: MemberId,
    },
    UnknownChannel {
        channel_id: ChannelId,
    },
    ChannelAlreadyOpen {
        channel_id: ChannelId,
    },
    MemberNotInChannel {
        member_id: MemberId,
        channel_id: ChannelId,
    },
    NotChannelOpener {
        member_id: MemberId,
        channel_id: ChannelId,
    },
    ChannelDataUnsupported {
        channel_id: ChannelId,
        kind: ChannelKind,
    },
    ChannelTransferUpdateUnsupported {
        channel_id: ChannelId,
        kind: ChannelKind,
    },
    HostCannotLeaveRoom,
}

impl fmt::Display for RoomEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{error}"),
            Self::UnknownMember { member_id } => {
                write!(f, "unknown room member {}", member_id.as_str())
            }
            Self::UnknownChannel { channel_id } => {
                write!(f, "unknown channel {}", channel_id.as_str())
            }
            Self::ChannelAlreadyOpen { channel_id } => {
                write!(f, "channel {} is already open", channel_id.as_str())
            }
            Self::MemberNotInChannel {
                member_id,
                channel_id,
            } => {
                write!(
                    f,
                    "member {} is not part of channel {}",
                    member_id.as_str(),
                    channel_id.as_str()
                )
            }
            Self::NotChannelOpener {
                member_id,
                channel_id,
            } => {
                write!(
                    f,
                    "member {} did not open channel {}",
                    member_id.as_str(),
                    channel_id.as_str()
                )
            }
            Self::ChannelDataUnsupported { channel_id, kind } => {
                write!(
                    f,
                    "channel {} of kind {:?} does not support inline channel data",
                    channel_id.as_str(),
                    kind
                )
            }
            Self::ChannelTransferUpdateUnsupported { channel_id, kind } => {
                write!(
                    f,
                    "channel {} of kind {:?} does not support transfer updates",
                    channel_id.as_str(),
                    kind
                )
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
    use skyffla_protocol::room::{
        ChannelKind, MachineEvent, TransferDigest, TransferItemKind, TransferOffer,
    };

    use super::*;

    fn file_transfer_offer() -> TransferOffer {
        TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: Some(TransferDigest {
                algorithm: "blake3".into(),
                value: "feedbeef".into(),
            }),
        }
    }

    fn provisional_file_transfer_offer() -> TransferOffer {
        TransferOffer {
            item_kind: TransferItemKind::File,
            integrity: None,
        }
    }

    fn member_id(value: &str) -> MemberId {
        MemberId::new(value).expect("valid member id")
    }

    fn room_id(value: &str) -> RoomId {
        RoomId::new(value).expect("valid room id")
    }

    fn channel_id(value: &str) -> ChannelId {
        ChannelId::new(value).expect("valid channel id")
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
        assert!(events
            .iter()
            .any(|event| event.recipient == member_id("m1")));
        assert!(events
            .iter()
            .any(|event| event.recipient == gamma.member.member_id));
        assert!(events
            .iter()
            .all(|event| matches!(event.event, MachineEvent::Chat { .. })));
        assert!(events.iter().all(|event| {
            matches!(
                &event.event,
                MachineEvent::Chat { from_name, .. } if from_name == "beta"
            )
        }));
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

        room.leave(&gamma.member.member_id, None)
            .expect("gamma leaves");

        let events = room
            .send_chat(&beta.member.member_id, Route::All, "after leave")
            .expect("chat succeeds");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].recipient, member_id("m1"));
    }

    #[test]
    fn open_channel_tracks_participants_and_routes_to_target() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let _beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");
        let host_member = room.host_member().clone();

        let opened = room
            .open_channel(
                &host_member,
                channel_id("c1"),
                ChannelKind::Machine,
                Route::Member {
                    member_id: gamma.member.member_id.clone(),
                },
                Some("demo".into()),
                None,
                None,
                None,
            )
            .expect("channel opens");

        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].recipient, gamma.member.member_id);
        assert!(matches!(
            opened[0].event,
            MachineEvent::ChannelOpened { .. }
        ));

        let data = room
            .send_channel_data(&host_member, &channel_id("c1"), "hello".into())
            .expect("channel data routes");
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].recipient, member_id("m3"));
        assert!(matches!(data[0].event, MachineEvent::ChannelData { .. }));
    }

    #[test]
    fn file_channel_accept_routes_but_inline_data_stays_rejected() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("file1"),
            ChannelKind::File,
            Route::Member {
                member_id: beta.member.member_id.clone(),
            },
            Some("report.txt".into()),
            Some(5),
            Some("text/plain".into()),
            Some(file_transfer_offer()),
        )
        .expect("file channel opens");

        let accepted = room
            .accept_channel(&beta.member.member_id, &channel_id("file1"))
            .expect("file channel accepts");
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].recipient, member_id("m1"));

        let late_data =
            room.send_channel_data(&beta.member.member_id, &channel_id("file1"), "raw".into());
        assert!(matches!(
            late_data,
            Err(RoomEngineError::ChannelDataUnsupported { .. })
        ));
    }

    #[test]
    fn file_channel_transfer_update_routes_and_updates_room_state() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("file1"),
            ChannelKind::File,
            Route::Member {
                member_id: beta.member.member_id.clone(),
            },
            Some("report.txt".into()),
            Some(5),
            Some("text/plain".into()),
            Some(provisional_file_transfer_offer()),
        )
        .expect("file channel opens");

        let routed = room
            .update_channel_transfer(
                &host_member,
                &channel_id("file1"),
                Some(5),
                file_transfer_offer(),
            )
            .expect("transfer update succeeds");

        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].recipient, beta.member.member_id);
        assert!(matches!(
            routed[0].event,
            MachineEvent::ChannelTransferFinalized { .. }
        ));
        assert_eq!(
            room.channel(&channel_id("file1"))
                .expect("channel present")
                .transfer,
            Some(file_transfer_offer())
        );
    }

    #[test]
    fn channel_data_reaches_other_participants_only() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");
        room.open_channel(
            &beta.member.member_id,
            channel_id("c1"),
            ChannelKind::Machine,
            Route::Member {
                member_id: gamma.member.member_id.clone(),
            },
            None,
            None,
            None,
            None,
        )
        .expect("channel opens");

        let routed = room
            .send_channel_data(&gamma.member.member_id, &channel_id("c1"), "pong".into())
            .expect("channel data routes");

        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].recipient, beta.member.member_id);
        assert!(matches!(routed[0].event, MachineEvent::ChannelData { .. }));
    }

    #[test]
    fn rejecting_two_member_channel_removes_it_from_future_routing() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("c1"),
            ChannelKind::Machine,
            Route::Member {
                member_id: beta.member.member_id.clone(),
            },
            None,
            None,
            None,
            None,
        )
        .expect("channel opens");

        let rejected = room
            .reject_channel(
                &beta.member.member_id,
                &channel_id("c1"),
                Some("nope".into()),
            )
            .expect("channel rejects");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].recipient, member_id("m1"));
        assert!(matches!(
            rejected[0].event,
            MachineEvent::ChannelRejected { .. }
        ));

        let err = room
            .send_channel_data(
                &beta.member.member_id,
                &channel_id("c1"),
                "still open?".into(),
            )
            .expect_err("rejected channel should be gone");
        assert!(matches!(err, RoomEngineError::UnknownChannel { .. }));
    }

    #[test]
    fn closing_two_member_channel_removes_it_from_future_routing() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("c1"),
            ChannelKind::Machine,
            Route::Member {
                member_id: beta.member.member_id.clone(),
            },
            None,
            None,
            None,
            None,
        )
        .expect("channel opens");

        let closed = room
            .close_channel(&host_member, &channel_id("c1"), Some("done".into()))
            .expect("channel closes");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].recipient, beta.member.member_id);
        assert!(matches!(
            closed[0].event,
            MachineEvent::ChannelClosed { .. }
        ));

        let err = room
            .send_channel_data(&host_member, &channel_id("c1"), "after close".into())
            .expect_err("closed channel should be gone");
        assert!(matches!(err, RoomEngineError::UnknownChannel { .. }));
    }

    #[test]
    fn broadcast_channel_includes_all_current_members() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");

        let opened = room
            .open_channel(
                &beta.member.member_id,
                channel_id("c1"),
                ChannelKind::Machine,
                Route::All,
                None,
                None,
                None,
                None,
            )
            .expect("channel opens");

        assert_eq!(opened.len(), 2);
        assert!(opened
            .iter()
            .any(|event| event.recipient == member_id("m1")));
        assert!(opened
            .iter()
            .any(|event| event.recipient == gamma.member.member_id));

        let routed = room
            .send_channel_data(&gamma.member.member_id, &channel_id("c1"), "fanout".into())
            .expect("channel data routes");
        assert_eq!(routed.len(), 2);
        assert!(routed
            .iter()
            .any(|event| event.recipient == member_id("m1")));
        assert!(routed
            .iter()
            .any(|event| event.recipient == beta.member.member_id));
    }

    #[test]
    fn rejecting_one_broadcast_participant_keeps_channel_for_others() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("c1"),
            ChannelKind::File,
            Route::All,
            Some("report.txt".into()),
            Some(12),
            Some("text/plain".into()),
            Some(file_transfer_offer()),
        )
        .expect("channel opens");

        let rejected = room
            .reject_channel(
                &gamma.member.member_id,
                &channel_id("c1"),
                Some("busy".into()),
            )
            .expect("gamma can reject");
        assert_eq!(rejected.len(), 2);
        assert!(rejected.iter().any(|event| event.recipient == host_member));
        assert!(rejected
            .iter()
            .any(|event| event.recipient == beta.member.member_id));

        let accepted = room
            .accept_channel(&beta.member.member_id, &channel_id("c1"))
            .expect("beta can still accept");
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].recipient, host_member);
    }

    #[test]
    fn closing_one_broadcast_participant_keeps_channel_for_others() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let gamma = room.join("gamma", None).expect("gamma joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("c1"),
            ChannelKind::Machine,
            Route::All,
            None,
            None,
            None,
            None,
        )
        .expect("channel opens");

        let closed = room
            .close_channel(
                &gamma.member.member_id,
                &channel_id("c1"),
                Some("done".into()),
            )
            .expect("gamma can close");
        assert_eq!(closed.len(), 2);
        assert!(closed.iter().any(|event| event.recipient == host_member));
        assert!(closed
            .iter()
            .any(|event| event.recipient == beta.member.member_id));

        let routed = room
            .send_channel_data(&host_member, &channel_id("c1"), "still open".into())
            .expect("channel should remain for host and beta");
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].recipient, beta.member.member_id);
    }

    #[test]
    fn file_channels_require_transfer_metadata_and_reject_inline_data() {
        let mut room = RoomEngine::new(room_id("warehouse"), "alpha", None).expect("room");
        let beta = room.join("beta", None).expect("beta joins");
        let host_member = room.host_member().clone();

        room.open_channel(
            &host_member,
            channel_id("c1"),
            ChannelKind::File,
            Route::Member {
                member_id: beta.member.member_id.clone(),
            },
            Some("report.pdf".into()),
            Some(1024),
            Some("application/pdf".into()),
            Some(file_transfer_offer()),
        )
        .expect("file channel opens");

        let err = room
            .send_channel_data(&host_member, &channel_id("c1"), "raw bytes".into())
            .expect_err("file channels should not accept inline data");
        assert!(matches!(
            err,
            RoomEngineError::ChannelDataUnsupported { .. }
        ));
    }
}
