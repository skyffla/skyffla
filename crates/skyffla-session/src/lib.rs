pub mod room;

use std::collections::BTreeMap;
use std::fmt;

use skyffla_protocol::SessionMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPeer {
    pub session_id: String,
    pub peer_name: String,
    pub peer_fingerprint: Option<String>,
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
    Hosting { stream_id: String },
    Joining { stream_id: String },
    Connecting { stream_id: String },
    Negotiating { session_id: String },
    Interactive { session_id: String },
    Stdio { session_id: String },
    Machine { session_id: String },
    Closing,
    Closed,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    HostRequested { stream_id: String },
    JoinRequested { stream_id: String },
    TransportConnecting,
    PeerConnected { session_id: String },
    Negotiated {
        session_id: String,
        session_mode: SessionMode,
    },
    CloseRequested,
    Closed,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Offered,
    Accepted,
    Rejected,
    Streaming,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub struct SessionMachine {
    state: SessionState,
    transfers: BTreeMap<String, TransferState>,
}

impl SessionMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn transfers(&self) -> &BTreeMap<String, TransferState> {
        &self.transfers
    }

    pub fn transition(&mut self, event: SessionEvent) -> Result<&SessionState, SessionError> {
        let next_state = match (&self.state, event) {
            (SessionState::Idle, SessionEvent::HostRequested { stream_id }) => {
                SessionState::Hosting { stream_id }
            }
            (SessionState::Joining { .. }, SessionEvent::HostRequested { stream_id }) => {
                SessionState::Hosting { stream_id }
            }
            (SessionState::Idle, SessionEvent::JoinRequested { stream_id }) => {
                SessionState::Joining { stream_id }
            }
            (SessionState::Hosting { stream_id }, SessionEvent::TransportConnecting)
            | (SessionState::Joining { stream_id }, SessionEvent::TransportConnecting) => {
                SessionState::Connecting {
                    stream_id: stream_id.clone(),
                }
            }
            (SessionState::Connecting { .. }, SessionEvent::PeerConnected { session_id }) => {
                SessionState::Negotiating { session_id }
            }
            (
                SessionState::Negotiating { .. },
                SessionEvent::Negotiated {
                    session_id,
                    session_mode,
                },
            ) => match session_mode {
                SessionMode::Stdio => SessionState::Stdio { session_id },
                SessionMode::Machine => SessionState::Machine { session_id },
                SessionMode::Interactive | SessionMode::Message => {
                    SessionState::Interactive { session_id }
                }
            },
            (SessionState::Interactive { .. }, SessionEvent::CloseRequested)
            | (SessionState::Stdio { .. }, SessionEvent::CloseRequested)
            | (SessionState::Machine { .. }, SessionEvent::CloseRequested)
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

    pub fn register_transfer(
        &mut self,
        transfer_id: impl Into<String>,
    ) -> Result<(), SessionError> {
        let transfer_id = transfer_id.into();
        if self.transfers.contains_key(&transfer_id) {
            return Err(SessionError::DuplicateTransfer { transfer_id });
        }

        self.transfers.insert(transfer_id, TransferState::Offered);
        Ok(())
    }

    pub fn update_transfer(
        &mut self,
        transfer_id: &str,
        next_state: TransferState,
    ) -> Result<(), SessionError> {
        let current =
            self.transfers
                .get_mut(transfer_id)
                .ok_or_else(|| SessionError::UnknownTransfer {
                    transfer_id: transfer_id.to_string(),
                })?;

        if !is_valid_transfer_transition(current, &next_state) {
            return Err(SessionError::InvalidTransferTransition {
                transfer_id: transfer_id.to_string(),
                from: current.clone(),
                to: next_state.clone(),
            });
        }

        *current = next_state;
        Ok(())
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            transfers: BTreeMap::new(),
        }
    }
}

pub fn state_changed_event(state: &SessionState) -> RuntimeEvent {
    RuntimeEvent::StateChanged(state.clone())
}

fn is_valid_transfer_transition(from: &TransferState, to: &TransferState) -> bool {
    matches!(
        (from, to),
        (TransferState::Offered, TransferState::Accepted)
            | (TransferState::Offered, TransferState::Rejected)
            | (TransferState::Offered, TransferState::Cancelled)
            | (TransferState::Offered, TransferState::Failed)
            | (TransferState::Accepted, TransferState::Streaming)
            | (TransferState::Accepted, TransferState::Cancelled)
            | (TransferState::Accepted, TransferState::Failed)
            | (TransferState::Streaming, TransferState::Completed)
            | (TransferState::Streaming, TransferState::Cancelled)
            | (TransferState::Streaming, TransferState::Failed)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    InvalidTransition {
        from: SessionState,
        event: SessionEvent,
    },
    DuplicateTransfer {
        transfer_id: String,
    },
    UnknownTransfer {
        transfer_id: String,
    },
    InvalidTransferTransition {
        transfer_id: String,
        from: TransferState,
        to: TransferState,
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
            Self::DuplicateTransfer { transfer_id } => {
                write!(f, "transfer {transfer_id} is already registered")
            }
            Self::UnknownTransfer { transfer_id } => write!(f, "unknown transfer {transfer_id}"),
            Self::InvalidTransferTransition {
                transfer_id,
                from,
                to,
            } => write!(
                f,
                "invalid transfer transition for {transfer_id} from {from:?} to {to:?}"
            ),
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_supports_interactive_flow() {
        let mut machine = SessionMachine::new();
        machine
            .transition(SessionEvent::HostRequested {
                stream_id: "demo".into(),
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
                session_mode: SessionMode::Interactive,
            })
            .expect("interactive negotiation should be accepted");

        assert_eq!(
            machine.state(),
            &SessionState::Interactive {
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
                stream_id: "demo".into(),
            })
            .expect("join should be accepted");
        machine
            .transition(SessionEvent::HostRequested {
                stream_id: "demo".into(),
            })
            .expect("join should be able to claim the stream");

        assert_eq!(
            machine.state(),
            &SessionState::Hosting {
                stream_id: "demo".into()
            }
        );
    }

    #[test]
    fn transfer_lifecycle_tracks_expected_states() {
        let mut machine = SessionMachine::new();
        machine
            .register_transfer("t1")
            .expect("offer should register");
        machine
            .update_transfer("t1", TransferState::Accepted)
            .expect("accept should succeed");
        machine
            .update_transfer("t1", TransferState::Streaming)
            .expect("streaming should succeed");
        machine
            .update_transfer("t1", TransferState::Completed)
            .expect("completion should succeed");

        assert_eq!(
            machine.transfers().get("t1"),
            Some(&TransferState::Completed)
        );
    }

    #[test]
    fn transfer_lifecycle_rejects_skipped_states() {
        let mut machine = SessionMachine::new();
        machine
            .register_transfer("t1")
            .expect("offer should register");

        let result = machine.update_transfer("t1", TransferState::Completed);
        assert!(matches!(
            result,
            Err(SessionError::InvalidTransferTransition { .. })
        ));
    }
}
