use std::sync::mpsc::{self as std_mpsc, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use arboard::Clipboard;
use tokio::sync::mpsc;

use crate::cli_error::CliError;

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(400);

pub(crate) enum ClipboardMode {
    Watch,
    ApplyOnly,
}

pub(crate) enum ClipboardEvent {
    LocalTextChanged { text: String },
    Warning { message: String },
}

enum ClipboardCommand {
    SetText(String),
    Shutdown,
}

pub(crate) struct ClipboardBackend {
    command_tx: Sender<ClipboardCommand>,
    event_rx: mpsc::UnboundedReceiver<ClipboardEvent>,
    join_handle: Option<JoinHandle<()>>,
}

impl ClipboardBackend {
    pub(crate) fn start(mode: ClipboardMode) -> Result<Self, CliError> {
        let (command_tx, command_rx) = std_mpsc::channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
        let join_handle = thread::spawn(move || {
            clipboard_worker(mode, command_rx, event_tx, startup_tx);
        });

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                command_tx,
                event_rx,
                join_handle: Some(join_handle),
            }),
            Ok(Err(message)) => {
                let _ = join_handle.join();
                Err(CliError::runtime(message))
            }
            Err(_) => {
                let _ = join_handle.join();
                Err(CliError::runtime(
                    "clipboard backend exited before initialization",
                ))
            }
        }
    }

    pub(crate) async fn recv_event(&mut self) -> Option<ClipboardEvent> {
        self.event_rx.recv().await
    }

    pub(crate) fn set_text(&self, text: String) -> Result<(), CliError> {
        self.command_tx
            .send(ClipboardCommand::SetText(text))
            .map_err(|_| CliError::runtime("clipboard backend is not available"))
    }
}

impl Drop for ClipboardBackend {
    fn drop(&mut self) {
        let _ = self.command_tx.send(ClipboardCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

fn clipboard_worker(
    mode: ClipboardMode,
    command_rx: Receiver<ClipboardCommand>,
    event_tx: mpsc::UnboundedSender<ClipboardEvent>,
    startup_tx: SyncSender<Result<(), String>>,
) {
    let mut clipboard = match Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            let _ = startup_tx.send(Err(format!(
                "failed to access the system clipboard: {error}"
            )));
            return;
        }
    };
    let _ = startup_tx.send(Ok(()));

    let watch_local = matches!(mode, ClipboardMode::Watch);
    let mut last_observed_text = read_clipboard_text(&mut clipboard);

    loop {
        match command_rx.recv_timeout(CLIPBOARD_POLL_INTERVAL) {
            Ok(ClipboardCommand::SetText(text)) => {
                if let Err(error) = clipboard.set_text(text.clone()) {
                    let _ = event_tx.send(ClipboardEvent::Warning {
                        message: format!("failed to write local clipboard: {error}"),
                    });
                } else {
                    last_observed_text = Some(text);
                }
            }
            Ok(ClipboardCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if !watch_local {
            continue;
        }

        let current_text = read_clipboard_text(&mut clipboard);
        if current_text == last_observed_text {
            continue;
        }
        last_observed_text = current_text.clone();

        let Some(text) = current_text else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        if event_tx
            .send(ClipboardEvent::LocalTextChanged { text })
            .is_err()
        {
            break;
        }
    }
}

fn read_clipboard_text(clipboard: &mut Clipboard) -> Option<String> {
    clipboard.get_text().ok()
}
