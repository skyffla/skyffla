use std::collections::{BTreeMap, BTreeSet};

use skyffla_protocol::room::{
    BlobRef, ChannelId, ChannelKind, MachineEvent, Member, MemberId, Route, TransferItemKind,
};
use skyffla_protocol::room_link::PeerLinkMessage;
use tokio::sync::mpsc;

use crate::cli_error::CliError;

#[derive(Debug)]
pub(super) struct JoinState {
    pub(super) self_member: Option<MemberId>,
    pub(super) host_member: Option<MemberId>,
    pub(super) members: BTreeMap<MemberId, Member>,
    pub(super) member_tickets: BTreeMap<MemberId, String>,
    pub(super) channels: BTreeMap<ChannelId, JoinChannelState>,
    pub(super) peer_links: BTreeMap<MemberId, mpsc::UnboundedSender<PeerLinkMessage>>,
    pub(super) local_name: String,
    pub(super) local_fingerprint: Option<String>,
    pub(super) local_ticket: String,
    pub(super) pending_host_ticket: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct JoinChannelState {
    pub(super) opener: MemberId,
    pub(super) kind: ChannelKind,
    pub(super) participants: BTreeSet<MemberId>,
    pub(super) item_kind: Option<TransferItemKind>,
    pub(super) name: Option<String>,
    pub(super) size: Option<u64>,
    pub(super) blob: Option<BlobRef>,
    pub(super) local_file_ready: bool,
}

#[derive(Debug, Clone)]
pub(super) struct PendingFileTransfer {
    pub(super) provider_ticket: String,
    pub(super) blob: BlobRef,
    pub(super) item_kind: TransferItemKind,
    pub(super) name: String,
    pub(super) size: Option<u64>,
}

#[derive(Debug, Clone)]
pub(super) struct ExportableFileTransfer {
    pub(super) blob: BlobRef,
    pub(super) item_kind: TransferItemKind,
    pub(super) name: String,
    pub(super) size: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct HostState {
    pub(super) ready_file_channels: BTreeSet<ChannelId>,
}

pub(super) fn apply_machine_event(state: &mut JoinState, event: &MachineEvent) {
    track_join_state(state, event);
    match event {
        MachineEvent::MemberSnapshot { members } => {
            state.members = members
                .iter()
                .cloned()
                .map(|member| (member.member_id.clone(), member))
                .collect();
        }
        MachineEvent::MemberJoined { member } => {
            state
                .members
                .insert(member.member_id.clone(), member.clone());
        }
        MachineEvent::MemberLeft { member_id, .. } => {
            state.members.remove(member_id);
            state.member_tickets.remove(member_id);
            state.peer_links.remove(member_id);
            prune_join_channels_for_member(state, member_id);
        }
        MachineEvent::RoomClosed { .. } => {
            state.members.clear();
            state.member_tickets.clear();
            state.channels.clear();
            state.peer_links.clear();
        }
        MachineEvent::ChannelOpened {
            channel_id,
            kind,
            from,
            to,
            name,
            size,
            blob,
            ..
        } => {
            if let Ok(participants) = event_participants(state, from, to) {
                state.channels.insert(
                    channel_id.clone(),
                    JoinChannelState {
                        opener: from.clone(),
                        kind: kind.clone(),
                        participants,
                        item_kind: blob.as_ref().map(|blob| match blob.format {
                            skyffla_protocol::room::BlobFormat::Blob => TransferItemKind::File,
                            skyffla_protocol::room::BlobFormat::Collection => {
                                TransferItemKind::Folder
                            }
                        }),
                        name: name.clone(),
                        size: *size,
                        blob: blob.clone(),
                        local_file_ready: state.self_member.as_ref() == Some(from),
                    },
                );
            }
        }
        MachineEvent::ChannelFileReady { channel_id, .. } => {
            if let Some(channel) = state.channels.get_mut(channel_id) {
                channel.local_file_ready = true;
            }
        }
        MachineEvent::ChannelClosed { channel_id, .. }
        | MachineEvent::ChannelRejected { channel_id, .. } => {
            if let Some(member_id) = event_actor_member(event) {
                remove_join_channel_participant(state, channel_id, &member_id);
            } else {
                state.channels.remove(channel_id);
            }
        }
        _ => {}
    }
}

pub(super) fn join_chat_recipients(
    self_member: &MemberId,
    route: &Route,
    members: &BTreeMap<MemberId, Member>,
) -> Result<Vec<MemberId>, CliError> {
    match route {
        Route::All => Ok(members
            .keys()
            .filter(|member_id| *member_id != self_member)
            .cloned()
            .collect()),
        Route::Member { member_id } => {
            if member_id == self_member {
                return Ok(vec![]);
            }
            if !members.contains_key(member_id) {
                return Err(CliError::runtime(format!(
                    "unknown member {}",
                    member_id.as_str()
                )));
            }
            Ok(vec![member_id.clone()])
        }
    }
}

pub(super) fn event_participants(
    state: &JoinState,
    from: &MemberId,
    to: &Route,
) -> Result<BTreeSet<MemberId>, CliError> {
    if !state.members.contains_key(from) {
        return Err(CliError::runtime(format!(
            "unknown member {}",
            from.as_str()
        )));
    }

    let mut participants = BTreeSet::from([from.clone()]);
    match to {
        Route::All => {
            participants.extend(state.members.keys().cloned());
        }
        Route::Member { member_id } => {
            if !state.members.contains_key(member_id) {
                return Err(CliError::runtime(format!(
                    "unknown member {}",
                    member_id.as_str()
                )));
            }
            participants.insert(member_id.clone());
        }
    }
    Ok(participants)
}

pub(super) fn join_channel_recipients(
    self_member: &MemberId,
    channel_id: &ChannelId,
    channels: &BTreeMap<ChannelId, JoinChannelState>,
) -> Result<Vec<MemberId>, CliError> {
    let channel = channels
        .get(channel_id)
        .ok_or_else(|| CliError::runtime(format!("unknown channel {}", channel_id.as_str())))?;
    if channel.kind == ChannelKind::File {
        return Err(CliError::runtime(format!(
            "channel {} is blob-backed and does not accept inline channel data",
            channel_id.as_str()
        )));
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    Ok(channel
        .participants
        .iter()
        .filter(|member_id| *member_id != self_member)
        .cloned()
        .collect())
}

pub(super) fn join_pending_file_transfer(
    state: &JoinState,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<Option<PendingFileTransfer>, CliError> {
    let Some(channel) = state.channels.get(channel_id) else {
        return Err(CliError::runtime(format!(
            "unknown channel {}",
            channel_id.as_str()
        )));
    };
    if channel.kind != ChannelKind::File || channel.local_file_ready {
        return Ok(None);
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    let blob = channel.blob.clone().ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })?;
    let provider_ticket = state
        .member_tickets
        .get(&channel.opener)
        .cloned()
        .ok_or_else(|| {
            CliError::runtime(format!(
                "no ticket known for file provider {}",
                channel.opener.as_str()
            ))
        })?;
    Ok(Some(PendingFileTransfer {
        provider_ticket,
        blob,
        item_kind: channel.item_kind.clone().unwrap_or(TransferItemKind::File),
        name: channel
            .name
            .clone()
            .unwrap_or_else(|| channel_id.as_str().to_string()),
        size: channel.size,
    }))
}

pub(super) fn join_exportable_file(
    state: &JoinState,
    self_member: &MemberId,
    channel_id: &ChannelId,
) -> Result<ExportableFileTransfer, CliError> {
    let channel = state
        .channels
        .get(channel_id)
        .ok_or_else(|| CliError::runtime(format!("unknown channel {}", channel_id.as_str())))?;
    if channel.kind != ChannelKind::File {
        return Err(CliError::runtime(format!(
            "channel {} is not a file channel",
            channel_id.as_str()
        )));
    }
    if !channel.participants.contains(self_member) {
        return Err(CliError::runtime(format!(
            "member {} is not part of channel {}",
            self_member.as_str(),
            channel_id.as_str()
        )));
    }
    if !channel.local_file_ready {
        return Err(CliError::runtime(format!(
            "file channel {} is not ready yet",
            channel_id.as_str()
        )));
    }
    let blob = channel.blob.clone().ok_or_else(|| {
        CliError::runtime(format!(
            "file channel {} is missing blob metadata",
            channel_id.as_str()
        ))
    })?;
    Ok(ExportableFileTransfer {
        blob,
        item_kind: channel.item_kind.clone().unwrap_or(TransferItemKind::File),
        name: channel
            .name
            .clone()
            .unwrap_or_else(|| channel_id.as_str().to_string()),
        size: channel.size,
    })
}

pub(super) fn apply_host_event(state: &mut HostState, event: &MachineEvent) {
    match event {
        MachineEvent::ChannelFileReady { channel_id, .. } => {
            state.ready_file_channels.insert(channel_id.clone());
        }
        MachineEvent::ChannelRejected { channel_id, .. }
        | MachineEvent::ChannelClosed { channel_id, .. } => {
            state.ready_file_channels.remove(channel_id);
        }
        _ => {}
    }
}

fn track_join_state(state: &mut JoinState, event: &MachineEvent) {
    match event {
        MachineEvent::RoomWelcome {
            self_member,
            host_member,
            ..
        } => {
            state.self_member = Some(self_member.clone());
            state.host_member = Some(host_member.clone());
            if let Some(ticket) = &state.pending_host_ticket {
                state
                    .member_tickets
                    .insert(host_member.clone(), ticket.clone());
            }
        }
        MachineEvent::MemberSnapshot { members } => {
            if let Some(host_member) = &state.host_member {
                if let Some(member) = members
                    .iter()
                    .find(|member| &member.member_id == host_member)
                {
                    state.host_member = Some(member.member_id.clone());
                }
            }
        }
        _ => {}
    }
}

fn event_actor_member(event: &MachineEvent) -> Option<MemberId> {
    match event {
        MachineEvent::ChannelAccepted { member_id, .. }
        | MachineEvent::ChannelRejected { member_id, .. }
        | MachineEvent::ChannelClosed { member_id, .. } => Some(member_id.clone()),
        _ => None,
    }
}

fn remove_join_channel_participant(
    state: &mut JoinState,
    channel_id: &ChannelId,
    member_id: &MemberId,
) {
    let self_member = state.self_member.clone();
    let should_remove = if let Some(channel) = state.channels.get_mut(channel_id) {
        channel.participants.remove(member_id);
        channel.participants.len() < 2
            || self_member
                .as_ref()
                .is_some_and(|self_member| !channel.participants.contains(self_member))
    } else {
        false
    };
    if should_remove {
        state.channels.remove(channel_id);
    }
}

fn prune_join_channels_for_member(state: &mut JoinState, member_id: &MemberId) {
    let channel_ids = state.channels.keys().cloned().collect::<Vec<_>>();
    for channel_id in channel_ids {
        remove_join_channel_participant(state, &channel_id, member_id);
    }
}
