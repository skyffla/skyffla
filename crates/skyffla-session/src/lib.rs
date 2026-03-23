pub mod room;

use std::fmt;

use skyffla_protocol::ProtocolVersion;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPeer {
    pub session_id: String,
    pub peer_name: String,
    pub peer_fingerprint: Option<String>,
    pub peer_ticket: Option<String>,
    pub file_transfer_version: Option<ProtocolVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    StateChanged(SessionState),
    HandshakeCompleted {
        peer: SessionPeer,
    },
    ConnectionStatus {
        mode: String,
        remote_addr: Option<String>,
    },
    PeerTrust {
        status: String,
        peer_name: String,
        peer_fingerprint: Option<String>,
        previous_name: Option<String>,
    },
    ChatSent {
        text: String,
    },
    ChatReceived {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Hosting { room_id: String },
    Joining { room_id: String },
    Connecting { room_id: String },
    Negotiating { session_id: String },
    Machine { session_id: String },
    Closing,
    Closed,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    HostRequested {
        room_id: String,
    },
    JoinRequested {
        room_id: String,
    },
    TransportConnecting,
    PeerConnected {
        session_id: String,
    },
    Negotiated {
        session_id: String,
    },
    CloseRequested,
    Closed,
    Failed {
        reason: String,
    },
}

#[derive(Debug)]
pub struct SessionMachine {
    state: SessionState,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn transition(&mut self, event: SessionEvent) -> Result<&SessionState, SessionError> {
        let next_state = match (&self.state, event) {
            (SessionState::Idle, SessionEvent::HostRequested { room_id }) => {
                SessionState::Hosting { room_id }
            }
            (SessionState::Joining { .. }, SessionEvent::HostRequested { room_id }) => {
                SessionState::Hosting { room_id }
            }
            (SessionState::Idle, SessionEvent::JoinRequested { room_id }) => {
                SessionState::Joining { room_id }
            }
            (SessionState::Hosting { room_id }, SessionEvent::TransportConnecting)
            | (SessionState::Joining { room_id }, SessionEvent::TransportConnecting) => {
                SessionState::Connecting {
                    room_id: room_id.clone(),
                }
            }
            (SessionState::Connecting { .. }, SessionEvent::PeerConnected { session_id }) => {
                SessionState::Negotiating { session_id }
            }
            (
                SessionState::Negotiating { .. },
                SessionEvent::Negotiated { session_id },
            ) => SessionState::Machine { session_id },
            (SessionState::Machine { .. }, SessionEvent::CloseRequested)
            | (SessionState::Hosting { .. }, SessionEvent::CloseRequested)
            | (SessionState::Joining { .. }, SessionEvent::CloseRequested)
            | (SessionState::Connecting { .. }, SessionEvent::CloseRequested)
            | (SessionState::Negotiating { .. }, SessionEvent::CloseRequested) => {
                SessionState::Closing
            }
            (SessionState::Closing, SessionEvent::Closed) => SessionState::Closed,
            (_, SessionEvent::Failed { reason }) => SessionState::Failed { reason },
            (state, event) => {
                return Err(SessionError::InvalidTransition {
                    from: state.clone(),
                    event,
                });
            }
        };

        self.state = next_state;
        Ok(&self.state)
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
        }
    }
}

pub fn state_changed_event(state: &SessionState) -> RuntimeEvent {
    RuntimeEvent::StateChanged(state.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    InvalidTransition {
        from: SessionState,
        event: SessionEvent,
    },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, event } => {
                write!(
                    f,
                    "invalid session transition from {from:?} with event {event:?}"
                )
            }
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_supports_machine_flow() {
        let mut machine = SessionMachine::new();
        machine
            .transition(SessionEvent::HostRequested {
                room_id: "demo".into(),
            })
            .expect("host should be accepted");
        machine
            .transition(SessionEvent::TransportConnecting)
            .expect("connecting should be accepted");
        machine
            .transition(SessionEvent::PeerConnected {
                session_id: "s1".into(),
            })
            .expect("peer connection should be accepted");
        machine
            .transition(SessionEvent::Negotiated {
                session_id: "s1".into(),
            })
            .expect("machine negotiation should be accepted");

        assert_eq!(
            machine.state(),
            &SessionState::Machine {
                session_id: "s1".into(),
            }
        );
    }

    #[test]
    fn state_machine_rejects_invalid_transition() {
        let mut machine = SessionMachine::new();
        let result = machine.transition(SessionEvent::TransportConnecting);
        assert!(matches!(
            result,
            Err(SessionError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn state_machine_allows_join_to_promote_into_hosting() {
        let mut machine = SessionMachine::new();
        machine
            .transition(SessionEvent::JoinRequested {
                room_id: "demo".into(),
            })
            .expect("join should be accepted");
        machine
            .transition(SessionEvent::HostRequested {
                room_id: "demo".into(),
            })
            .expect("join should be able to claim the room");

        assert_eq!(
            machine.state(),
            &SessionState::Hosting {
                room_id: "demo".into()
            }
        );
    }
}
