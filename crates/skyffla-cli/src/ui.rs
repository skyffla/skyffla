use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen, ScrollUp,
};
use crossterm::{execute, queue};
use skyffla_protocol::Offer;

use crate::accept_policy::AutoAcceptPolicy;
use crate::local_state::{load_local_state, local_state_file_path, update_local_state};

pub(crate) enum UserInput {
    Chat(String),
    SendFile(PathBuf),
    SendClipboard,
    Accept,
    Reject,
    Cancel(Option<String>),
    AutoAccept(Option<bool>),
    Help,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransferStateUi {
    Pending,
    AwaitingDecision,
    Streaming,
    Completed,
    Rejected,
    Cancelled,
}

#[derive(Clone, Debug)]
pub(crate) struct TransferUi {
    pub(crate) id: String,
    pub(crate) state: TransferStateUi,
    pub(crate) bytes_done: u64,
    pub(crate) bytes_total: Option<u64>,
}

struct EventLine {
    timestamp: String,
    text: String,
}

pub(crate) struct UiState {
    pub(crate) stream_id: String,
    pub(crate) peer_name: String,
    pub(crate) local_name: String,
    pub(crate) self_member_id: Option<String>,
    pub(crate) host_member_id: Option<String>,
    pub(crate) room_members: BTreeMap<String, String>,
    pub(crate) auto_accept_policy: AutoAcceptPolicy,
    pub(crate) auto_accept_source: String,
    events: Vec<EventLine>,
    pub(crate) transfers: Vec<TransferUi>,
    pub(crate) pending_offer: Option<Offer>,
    input_buffer: String,
    cursor_index: usize,
    input_history: Vec<String>,
    history_index: Option<usize>,
    draft_buffer: Option<String>,
    pub(crate) state_path: Option<PathBuf>,
    rendered_event_lines: usize,
    rendered_width: Option<usize>,
    next_event_row: u16,
}

pub(crate) struct TerminalUiGuard;

impl TerminalUiGuard {
    pub(crate) fn activate() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, Clear(ClearType::All))
            .context("failed to initialize terminal UI")?;
        Ok(Self)
    }
}

impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[r");
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = write!(stdout, "\r");
        let _ = stdout.flush();
    }
}

impl UiState {
    pub(crate) fn new(
        stream_id: &str,
        local_name: &str,
        peer_name: &str,
        auto_accept_policy: AutoAcceptPolicy,
        auto_accept_source: &str,
    ) -> Result<Self> {
        let state_path = local_state_file_path();
        let state = load_local_state(&state_path)?;
        Ok(Self {
            stream_id: stream_id.to_string(),
            peer_name: peer_name.to_string(),
            local_name: local_name.to_string(),
            self_member_id: None,
            host_member_id: None,
            room_members: BTreeMap::new(),
            auto_accept_policy,
            auto_accept_source: auto_accept_source.to_string(),
            events: Vec::new(),
            transfers: Vec::new(),
            pending_offer: None,
            input_buffer: String::new(),
            cursor_index: 0,
            input_history: state.history,
            history_index: None,
            draft_buffer: None,
            state_path,
            rendered_event_lines: 0,
            rendered_width: None,
            next_event_row: 0,
        })
    }

    pub(crate) fn system(&mut self, message: String) {
        self.push_event(message);
    }

    pub(crate) fn chat(&mut self, speaker: &str, text: &str) {
        self.push_event(format!("{speaker}: {text}"));
    }

    pub(crate) fn set_room_identity(
        &mut self,
        self_member_id: impl Into<String>,
        host_member_id: impl Into<String>,
    ) {
        self.self_member_id = Some(self_member_id.into());
        self.host_member_id = Some(host_member_id.into());
    }

    pub(crate) fn replace_room_members(&mut self, members: impl IntoIterator<Item = (String, String)>) {
        self.room_members = members.into_iter().collect();
    }

    pub(crate) fn upsert_room_member(
        &mut self,
        member_id: impl Into<String>,
        name: impl Into<String>,
    ) {
        self.room_members.insert(member_id.into(), name.into());
    }

    pub(crate) fn remove_room_member(&mut self, member_id: &str) {
        self.room_members.remove(member_id);
    }

    pub(crate) fn upsert_transfer(&mut self, transfer: TransferUi) {
        if let Some(existing) = self.transfers.iter_mut().find(|t| t.id == transfer.id) {
            *existing = transfer;
        } else {
            self.transfers.push(transfer);
        }
    }

    pub(crate) fn mark_transfer_streaming(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Streaming;
        }
    }

    pub(crate) fn mark_transfer_completed(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Completed;
        }
    }

    pub(crate) fn mark_transfer_rejected(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Rejected;
        }
    }

    pub(crate) fn mark_transfer_cancelled(&mut self, transfer_id: &str) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.state = TransferStateUi::Cancelled;
        }
    }

    pub(crate) fn cancellable_transfer_ids(&self) -> Vec<String> {
        self.transfers
            .iter()
            .filter(|transfer| {
                matches!(
                    transfer.state,
                    TransferStateUi::Pending
                        | TransferStateUi::AwaitingDecision
                        | TransferStateUi::Streaming
                )
            })
            .map(|transfer| transfer.id.clone())
            .collect()
    }

    pub(crate) fn has_transfer(&self, transfer_id: &str) -> bool {
        self.transfers
            .iter()
            .any(|transfer| transfer.id == transfer_id)
    }

    pub(crate) fn update_transfer_progress(
        &mut self,
        transfer_id: &str,
        done: u64,
        total: Option<u64>,
    ) {
        if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
            transfer.bytes_done = done;
            if total.is_some() {
                transfer.bytes_total = total;
            }
            if transfer.state == TransferStateUi::Pending
                || transfer.state == TransferStateUi::AwaitingDecision
            {
                transfer.state = TransferStateUi::Streaming;
            }
        }
    }

    pub(crate) fn render(&mut self) {
        let width = terminal_width();
        let height = terminal_height();
        let divider = "-".repeat(width);
        let prompt_row = height.saturating_sub(1) as u16;
        let (visible_input, cursor_col) = self.prompt_window(width);
        let header_lines = self.header_lines(width);
        let header_start = 1u16;
        let header_end = header_start + header_lines.len() as u16;
        let mut stdout = std::io::stdout();

        if self.rendered_width != Some(width) {
            self.rendered_width = Some(width);
            self.rendered_event_lines = 0;
            let _ = queue!(stdout, MoveTo(0, 0), Clear(ClearType::All));
            let _ = write!(stdout, "\x1b[1;{}r", prompt_row);
        }

        let _ = queue!(
            stdout,
            MoveTo(0, 0),
            Clear(ClearType::CurrentLine),
            Print(clip_line(&divider, width))
        );
        for (index, line) in header_lines.iter().enumerate() {
            let _ = queue!(
                stdout,
                MoveTo(0, header_start + index as u16),
                Clear(ClearType::CurrentLine),
                Print(clip_line(line, width))
            );
        }
        let _ = queue!(
            stdout,
            MoveTo(0, header_end),
            Clear(ClearType::CurrentLine),
            Print(clip_line(&divider, width))
        );
        self.next_event_row = header_end + 1;

        let event_lines = if self.events.is_empty() {
            vec!["[--:--:--] waiting for events".to_string()]
        } else {
            self.render_event_lines(width)
        };
        for line in event_lines.iter().skip(self.rendered_event_lines) {
            if self.next_event_row >= prompt_row {
                let _ = queue!(stdout, ScrollUp(1));
                self.next_event_row = prompt_row.saturating_sub(1);
            }
            let _ = queue!(
                stdout,
                MoveTo(0, self.next_event_row),
                Clear(ClearType::CurrentLine),
                Print(clip_line(line, width))
            );
            self.next_event_row = self.next_event_row.saturating_add(1);
        }
        self.rendered_event_lines = event_lines.len();

        let _ = queue!(
            stdout,
            MoveTo(0, prompt_row),
            Clear(ClearType::CurrentLine),
            Print(format!("> {visible_input}")),
            MoveTo(cursor_col as u16, prompt_row),
            Show
        );
        let _ = stdout.flush();
    }

    pub(crate) fn auto_accept_status_line(&self) -> String {
        format!(
            "auto-accept effective: {} ({})",
            self.auto_accept_policy.describe(),
            self.auto_accept_source
        )
    }

    pub(crate) fn handle_key_event(&mut self, key: KeyEvent) -> Option<String> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some("/quit".to_string());
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_index = 0;
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_index = self.input_buffer.len();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input_buffer.truncate(self.cursor_index);
                self.history_index = None;
                self.draft_buffer = None;
            }
            KeyCode::Enter => {
                let submitted = self.input_buffer.trim().to_string();
                self.history_index = None;
                self.draft_buffer = None;
                self.cursor_index = 0;
                let previous = std::mem::take(&mut self.input_buffer);
                if !submitted.is_empty() {
                    self.record_history(previous);
                    return Some(submitted);
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.history_index = None;
                self.draft_buffer = None;
                self.input_buffer.insert(self.cursor_index, c);
                self.cursor_index += c.len_utf8();
            }
            KeyCode::Backspace => {
                if self.cursor_index > 0 {
                    let previous = previous_boundary(&self.input_buffer, self.cursor_index);
                    self.input_buffer.drain(previous..self.cursor_index);
                    self.cursor_index = previous;
                    self.history_index = None;
                    self.draft_buffer = None;
                }
            }
            KeyCode::Delete => {
                if self.cursor_index < self.input_buffer.len() {
                    let next = next_boundary(&self.input_buffer, self.cursor_index);
                    self.input_buffer.drain(self.cursor_index..next);
                    self.history_index = None;
                    self.draft_buffer = None;
                }
            }
            KeyCode::Left => {
                if self.cursor_index > 0 {
                    self.cursor_index = previous_boundary(&self.input_buffer, self.cursor_index);
                }
            }
            KeyCode::Right => {
                if self.cursor_index < self.input_buffer.len() {
                    self.cursor_index = next_boundary(&self.input_buffer, self.cursor_index);
                }
            }
            KeyCode::Up => self.history_up(),
            KeyCode::Down => self.history_down(),
            KeyCode::Home => self.cursor_index = 0,
            KeyCode::End => self.cursor_index = self.input_buffer.len(),
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.input_buffer.is_empty() =>
            {
                return Some("/quit".to_string());
            }
            _ => {}
        }

        None
    }

    fn render_event_lines(&self, width: usize) -> Vec<String> {
        self.events
            .iter()
            .flat_map(|event| {
                wrap_prefixed_lines(&format!("[{}] ", event.timestamp), &event.text, width)
            })
            .collect()
    }

    fn push_event(&mut self, text: String) {
        self.events.push(EventLine {
            timestamp: compact_timestamp(),
            text,
        });
    }

    fn record_history(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        self.input_history.push(line);
        if self.input_history.len() > 500 {
            let overflow = self.input_history.len() - 500;
            self.input_history.drain(0..overflow);
        }
        if let Err(error) = update_local_state(&self.state_path, |state| {
            state.history = self.input_history.clone();
        }) {
            self.system(format!("failed to persist command history: {error:#}"));
        }
    }

    fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        let next_index = match self.history_index {
            Some(0) => 0,
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft_buffer = Some(self.input_buffer.clone());
                self.input_history.len() - 1
            }
        };
        self.history_index = Some(next_index);
        self.input_buffer = self.input_history[next_index].clone();
        self.cursor_index = self.input_buffer.len();
    }

    fn history_down(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };

        if index + 1 < self.input_history.len() {
            let next_index = index + 1;
            self.history_index = Some(next_index);
            self.input_buffer = self.input_history[next_index].clone();
        } else {
            self.history_index = None;
            self.input_buffer = self.draft_buffer.take().unwrap_or_default();
        }
        self.cursor_index = self.input_buffer.len();
    }

    fn prompt_window(&self, width: usize) -> (String, usize) {
        let prompt_space = width.saturating_sub(2).max(1);
        let buffer_chars: Vec<char> = self.input_buffer.chars().collect();
        let cursor_chars = self.input_buffer[..self.cursor_index].chars().count();

        if buffer_chars.len() <= prompt_space {
            return (self.input_buffer.clone(), 2 + cursor_chars);
        }

        let mut start = cursor_chars.saturating_sub(prompt_space.saturating_sub(1));
        if start + prompt_space > buffer_chars.len() {
            start = buffer_chars.len().saturating_sub(prompt_space);
        }
        let end = (start + prompt_space).min(buffer_chars.len());
        let visible: String = buffer_chars[start..end].iter().collect();
        let cursor_col = 2 + cursor_chars.saturating_sub(start);
        (visible, cursor_col)
    }

    fn header_lines(&self, width: usize) -> Vec<String> {
        let self_member = self.self_member_id.as_deref().unwrap_or("?");
        let host_member = self.host_member_id.as_deref().unwrap_or("?");
        let members = if self.room_members.is_empty() {
            "members: (waiting)".to_string()
        } else {
            let mut parts = Vec::new();
            for (member_id, name) in &self.room_members {
                let marker = if self.self_member_id.as_deref() == Some(member_id.as_str()) {
                    "*"
                } else if self.host_member_id.as_deref() == Some(member_id.as_str()) {
                    "^"
                } else {
                    ""
                };
                parts.push(format!("{member_id}{marker}:{name}"));
            }
            format!("members: {}", parts.join(", "))
        };
        vec![
            clip_line(
                &format!(
                    "room={} you={} self={} host={} peer={}",
                    self.stream_id, self.local_name, self_member, host_member, self.peer_name
                ),
                width,
            ),
            clip_line(&members, width),
        ]
    }
}

pub(crate) fn parse_user_input(input: &str) -> UserInput {
    let trimmed = input.trim();
    match trimmed {
        "q" => return UserInput::Quit,
        "y" => return UserInput::Accept,
        "n" => return UserInput::Reject,
        _ => {}
    }
    if trimmed == "/quit" {
        UserInput::Quit
    } else if trimmed == "/help" {
        UserInput::Help
    } else if trimmed == "/clip" {
        UserInput::SendClipboard
    } else if trimmed == "/cancel" {
        UserInput::Cancel(None)
    } else if let Some(transfer_id) = trimmed.strip_prefix("/cancel ") {
        UserInput::Cancel(Some(transfer_id.trim().to_string()))
    } else if trimmed == "/autoaccept" {
        UserInput::AutoAccept(None)
    } else if trimmed == "/autoaccept on" {
        UserInput::AutoAccept(Some(true))
    } else if trimmed == "/autoaccept off" {
        UserInput::AutoAccept(Some(false))
    } else if trimmed == "/accept" {
        UserInput::Accept
    } else if trimmed == "/reject" {
        UserInput::Reject
    } else if let Some(path) = trimmed.strip_prefix("/send ") {
        UserInput::SendFile(expand_user_path(path.trim()))
    } else {
        UserInput::Chat(trimmed.to_string())
    }
}

pub(crate) fn help_lines() -> &'static [&'static str] {
    &[
        "commands:",
        "/help  show this help",
        "/send <path>  offer a file or folder",
        "/clip  offer clipboard text",
        "/accept  accept the pending file offer",
        "/reject  reject the pending file offer",
        "/cancel [id]  cancel an active transfer",
        "/autoaccept on|off  set the persisted default for file and clipboard offers",
        "/quit  close the session",
        "shortcuts: q quit, y accept, n reject, ctrl+c close",
        "editing: up/down history, ctrl+a line start, ctrl+e line end, ctrl+k kill to end",
    ]
}

pub(crate) fn resolve_cancel_target(
    ui: &UiState,
    requested: Option<&str>,
) -> std::result::Result<Option<String>, String> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(transfer_id) => {
            if !ui.has_transfer(transfer_id) {
                return Err(format!("unknown transfer {}", transfer_id));
            }
            Ok(Some(transfer_id.to_string()))
        }
        None => {
            let cancellable = ui.cancellable_transfer_ids();
            match cancellable.len() {
                0 => Ok(None),
                1 => Ok(cancellable.into_iter().next()),
                _ => Err("multiple active transfers; use /cancel <transfer-id>".to_string()),
            }
        }
    }
}

fn expand_user_path(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(input));
    }

    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(input)
}

fn wrap_prefixed_lines(prefix: &str, text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![prefix.to_string()];
    }

    let available = width.saturating_sub(prefix.len()).max(8);
    let indent = " ".repeat(prefix.len());
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let next_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if next_len > available && !current.is_empty() {
            if lines.is_empty() {
                lines.push(format!("{prefix}{current}"));
            } else {
                lines.push(format!("{indent}{current}"));
            }
            current.clear();
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }

    if current.is_empty() {
        lines.push(prefix.to_string());
    } else if lines.is_empty() {
        lines.push(format!("{prefix}{current}"));
    } else {
        lines.push(format!("{indent}{current}"));
    }

    lines
}

fn terminal_width() -> usize {
    terminal_size()
        .map(|(cols, _)| cols as usize)
        .ok()
        .filter(|cols| *cols >= 20)
        .unwrap_or(72)
}

fn terminal_height() -> usize {
    terminal_size()
        .map(|(_, rows)| rows as usize)
        .ok()
        .filter(|rows| *rows >= 8)
        .unwrap_or(24)
}

fn previous_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| index + offset)
        .unwrap_or(text.len())
}

fn compact_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() % 86_400)
        .unwrap_or(0);
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

fn clip_line(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}
