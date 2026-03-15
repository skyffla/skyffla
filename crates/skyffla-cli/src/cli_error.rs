use std::fmt;
use std::process::ExitCode;

use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CliExitCode {
    Runtime = 1,
    Usage = 2,
    Rendezvous = 10,
    Transport = 11,
    Protocol = 12,
    TransferCancelled = 21,
    PeerError = 22,
    LocalIo = 23,
}

impl CliExitCode {
    pub(crate) fn as_exit_code(self) -> ExitCode {
        ExitCode::from(self as u8)
    }
}

#[derive(Debug)]
pub(crate) struct CliError {
    exit_code: CliExitCode,
    code: &'static str,
    message: String,
}

impl CliError {
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::Usage, "usage_error", message)
    }

    pub(crate) fn rendezvous(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::Rendezvous, "rendezvous_error", message)
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::Transport, "transport_error", message)
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::Protocol, "protocol_error", message)
    }

    pub(crate) fn cancelled(message: impl Into<String>) -> Self {
        Self::new(
            CliExitCode::TransferCancelled,
            "transfer_cancelled",
            message,
        )
    }

    pub(crate) fn peer(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::PeerError, "peer_error", message)
    }

    pub(crate) fn local_io(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::LocalIo, "local_io_error", message)
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        Self::new(CliExitCode::Runtime, "runtime_error", message)
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        self.exit_code.as_exit_code()
    }

    pub(crate) fn emit(&self, json_mode: bool) {
        if json_mode {
            eprintln!(
                "{}",
                json!({
                    "event": "error",
                    "code": self.code,
                    "message": self.message,
                    "exit_code": self.exit_code as u8,
                })
            );
        } else {
            eprintln!("Error: {}", self.message);
        }
    }

    fn new(exit_code: CliExitCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for CliError {}
