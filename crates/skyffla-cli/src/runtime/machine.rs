use anyhow::Context;
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use skyffla_protocol::room::{
    MachineCommand, MachineEvent, MemberId, RoomId, Route, MACHINE_PROTOCOL_VERSION,
};
use skyffla_session::room::{RoomEngine, RoomEngineError, RoutedEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::app::sink::EventSink;
use crate::cli_error::CliError;
use crate::net::framing::{read_framed, write_framed};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PeerMachineMessage {
    Command { command: MachineCommand },
    Event { event: MachineEvent },
}

#[derive(Debug)]
struct JoinState {
    self_member: Option<MemberId>,
    host_member: Option<MemberId>,
}

pub(crate) async fn run_machine_session(
    session_id: &str,
    sink: &EventSink,
    send: &mut SendStream,
    recv: &mut RecvStream,
    is_host: bool,
    local_name: &str,
    local_fingerprint: Option<String>,
    peer_name: &str,
    peer_fingerprint: Option<String>,
) -> Result<(), CliError> {
    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut input_closed = false;
    let mut peer_closed = false;
    let mut join_state = JoinState {
        self_member: None,
        host_member: None,
    };

    if is_host {
        let mut room = RoomEngine::new(
            RoomId::new(session_id).map_err(protocol_error)?,
            local_name.to_string(),
            local_fingerprint,
        )
        .map_err(room_error)?;

        let host_member = room.host_member().clone();
        emit_event(
            &mut stdout,
            &MachineEvent::RoomWelcome {
                protocol_version: MACHINE_PROTOCOL_VERSION,
                room_id: room.room_id().clone(),
                self_member: host_member.clone(),
                host_member: host_member.clone(),
            },
        )
        .await?;
        emit_event(
            &mut stdout,
            &MachineEvent::MemberSnapshot {
                members: room.members().values().cloned().collect(),
            },
        )
        .await?;

        let join_dispatch = room
            .join(peer_name.to_string(), peer_fingerprint)
            .map_err(room_error)?;
        let peer_member = join_dispatch.member.member_id.clone();

        for event in join_dispatch.to_joiner {
            send_peer_event(send, &event).await?;
        }
        for routed in join_dispatch.to_existing_members {
            if routed.recipient == host_member {
                emit_event(&mut stdout, &routed.event).await?;
            }
        }

        while !(input_closed && peer_closed) {
            tokio::select! {
                next_line = stdin_lines.next_line(), if !input_closed => {
                    match next_line
                        .context("failed reading machine command from stdin")
                        .map_err(|error| CliError::local_io(error.to_string()))? {
                        Some(line) => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            let command = parse_command(&line)?;
                            handle_host_command(
                                &mut room,
                                &host_member,
                                &peer_member,
                                command,
                                &mut stdout,
                                send,
                            )
                            .await?;
                        }
                        None => {
                            input_closed = true;
                            let _ = send.finish();
                        }
                    }
                }
                message = read_framed::<PeerMachineMessage>(recv, "machine peer message"), if !peer_closed => {
                    match message.map_err(|error| CliError::protocol(error.to_string()))? {
                        Some(PeerMachineMessage::Command { command }) => {
                            handle_host_command(
                                &mut room,
                                &peer_member,
                                &host_member,
                                command,
                                &mut stdout,
                                send,
                            )
                            .await?;
                        }
                        Some(PeerMachineMessage::Event { .. }) => {
                            return Err(CliError::protocol(
                                "host received machine event from peer; expected command",
                            ));
                        }
                        None => {
                            peer_closed = true;
                        }
                    }
                }
            }
        }
    } else {
        while !(input_closed && peer_closed) {
            tokio::select! {
                next_line = stdin_lines.next_line(), if !input_closed => {
                    match next_line
                        .context("failed reading machine command from stdin")
                        .map_err(|error| CliError::local_io(error.to_string()))? {
                        Some(line) => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            if join_state.self_member.is_none() {
                                emit_event(&mut stdout, &local_error("room_not_ready", "machine room is not ready yet")).await?;
                                continue;
                            }
                            let command = parse_command(&line)?;
                            send_peer_command(send, &command).await?;
                        }
                        None => {
                            input_closed = true;
                            let _ = send.finish();
                        }
                    }
                }
                message = read_framed::<PeerMachineMessage>(recv, "machine peer message"), if !peer_closed => {
                    match message.map_err(|error| CliError::protocol(error.to_string()))? {
                        Some(PeerMachineMessage::Event { event }) => {
                            track_join_state(&mut join_state, &event);
                            emit_event(&mut stdout, &event).await?;
                        }
                        Some(PeerMachineMessage::Command { .. }) => {
                            return Err(CliError::protocol(
                                "joiner received machine command from peer; expected event",
                            ));
                        }
                        None => {
                            peer_closed = true;
                        }
                    }
                }
            }
        }
    }

    sink.emit_json_event(serde_json::json!({
        "event": "machine_session_closed",
        "room_id": session_id,
        "role": if is_host { "host" } else { "join" },
    }));

    Ok(())
}

async fn handle_host_command(
    room: &mut RoomEngine,
    sender: &MemberId,
    peer_member: &MemberId,
    command: MachineCommand,
    stdout: &mut tokio::io::Stdout,
    send: &mut SendStream,
) -> Result<(), CliError> {
    command.validate().map_err(protocol_error)?;

    match command {
        MachineCommand::SendChat { to, text } => {
            let routed = room.send_chat(sender, to, text).map_err(room_error)?;
            deliver_routed_events(sender, peer_member, routed, stdout, send).await
        }
        MachineCommand::OpenChannel {
            channel_id,
            kind,
            to,
            name,
            size,
            mime,
        } => {
            let event = MachineEvent::ChannelOpened {
                channel_id,
                kind,
                from: sender.clone(),
                to: to.clone(),
                name,
                size,
                mime,
            };
            let routed = route_event(room, sender, to, event).map_err(room_error)?;
            deliver_routed_events(sender, peer_member, routed, stdout, send).await
        }
        MachineCommand::AcceptChannel { channel_id } => {
            deliver_channel_reply(
                sender,
                peer_member,
                stdout,
                send,
                MachineEvent::ChannelAccepted {
                    channel_id,
                    member_id: sender.clone(),
                },
            )
            .await
        }
        MachineCommand::RejectChannel { channel_id, reason } => {
            deliver_channel_reply(
                sender,
                peer_member,
                stdout,
                send,
                MachineEvent::ChannelRejected {
                    channel_id,
                    member_id: sender.clone(),
                    reason,
                },
            )
            .await
        }
        MachineCommand::SendChannelData { channel_id, body } => {
            deliver_channel_reply(
                sender,
                peer_member,
                stdout,
                send,
                MachineEvent::ChannelData {
                    channel_id,
                    from: sender.clone(),
                    body,
                },
            )
            .await
        }
        MachineCommand::CloseChannel { channel_id, reason } => {
            deliver_channel_reply(
                sender,
                peer_member,
                stdout,
                send,
                MachineEvent::ChannelClosed {
                    channel_id,
                    member_id: sender.clone(),
                    reason,
                },
            )
            .await
        }
    }
}

fn route_event(
    room: &RoomEngine,
    sender: &MemberId,
    route: Route,
    event: MachineEvent,
) -> Result<Vec<RoutedEvent>, RoomEngineError> {
    match route {
        Route::All => Ok(room
            .members()
            .keys()
            .filter(|member_id| *member_id != sender)
            .cloned()
            .map(|recipient| RoutedEvent {
                recipient,
                event: event.clone(),
            })
            .collect()),
        Route::Member { member_id } => {
            if room.members().contains_key(&member_id) {
                if &member_id == sender {
                    Ok(vec![])
                } else {
                    Ok(vec![RoutedEvent {
                        recipient: member_id,
                        event,
                    }])
                }
            } else {
                Err(RoomEngineError::UnknownMember {
                    member_id,
                })
            }
        }
    }
}

async fn deliver_routed_events(
    host_member: &MemberId,
    peer_member: &MemberId,
    routed_events: Vec<RoutedEvent>,
    stdout: &mut tokio::io::Stdout,
    send: &mut SendStream,
) -> Result<(), CliError> {
    for routed in routed_events {
        if &routed.recipient == host_member {
            emit_event(stdout, &routed.event).await?;
        } else if &routed.recipient == peer_member {
            send_peer_event(send, &routed.event).await?;
        }
    }
    Ok(())
}

async fn deliver_channel_reply(
    sender: &MemberId,
    peer_member: &MemberId,
    stdout: &mut tokio::io::Stdout,
    send: &mut SendStream,
    event: MachineEvent,
) -> Result<(), CliError> {
    if sender == peer_member {
        emit_event(stdout, &event).await
    } else {
        send_peer_event(send, &event).await
    }
}

fn parse_command(line: &str) -> Result<MachineCommand, CliError> {
    let command: MachineCommand =
        serde_json::from_str(line).map_err(|error| CliError::usage(error.to_string()))?;
    command.validate().map_err(protocol_error)?;
    Ok(command)
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
        }
        MachineEvent::MemberSnapshot { members } => {
            if let Some(host_member) = &state.host_member {
                if let Some(member) = members.iter().find(|member| &member.member_id == host_member) {
                    state.host_member = Some(member.member_id.clone());
                }
            }
        }
        _ => {}
    }
}

fn local_error(code: &str, message: &str) -> MachineEvent {
    MachineEvent::Error {
        code: code.into(),
        message: message.into(),
        channel_id: None,
    }
}

async fn emit_event(
    stdout: &mut tokio::io::Stdout,
    event: &MachineEvent,
) -> Result<(), CliError> {
    let line = serde_json::to_string(event).map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .write_all(line.as_bytes())
        .await
        .context("failed writing machine event to stdout")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .write_all(b"\n")
        .await
        .context("failed writing machine event newline")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    stdout
        .flush()
        .await
        .context("failed flushing machine event stdout")
        .map_err(|error| CliError::local_io(error.to_string()))?;
    Ok(())
}

async fn send_peer_command(send: &mut SendStream, command: &MachineCommand) -> Result<(), CliError> {
    write_framed(
        send,
        &PeerMachineMessage::Command {
            command: command.clone(),
        },
        "machine command",
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))
}

async fn send_peer_event(send: &mut SendStream, event: &MachineEvent) -> Result<(), CliError> {
    write_framed(
        send,
        &PeerMachineMessage::Event {
            event: event.clone(),
        },
        "machine event",
    )
    .await
    .map_err(|error| CliError::protocol(error.to_string()))
}

fn protocol_error(error: impl ToString) -> CliError {
    CliError::protocol(error.to_string())
}

fn room_error(error: impl ToString) -> CliError {
    CliError::runtime(error.to_string())
}
