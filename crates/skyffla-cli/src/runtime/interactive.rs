use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use iroh::endpoint::{RecvStream, SendStream};
use skyffla_transport::IrohConnection;
use tokio::sync::mpsc;

use crate::config::SessionConfig;
use crate::net::framing::read_envelope;
use crate::runtime::interactive_commands::handle_user_input;
use crate::runtime::interactive_control::{
    shutdown_interactive_session, wait_for_shutdown_signal, TransferRegistry,
};
use crate::runtime::interactive_peer::{handle_interactive_envelope, handle_transfer_task_event};
use crate::ui::{parse_user_input, TerminalUiGuard, UiState};

pub(crate) async fn run_interactive_chat_loop(
    config: &SessionConfig,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    ui: &mut UiState,
) -> Result<()> {
    let _terminal = TerminalUiGuard::activate()?;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let (task_tx, mut task_rx) = mpsc::unbounded_channel();
    let mut registry = TransferRegistry::new();

    thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if input_tx.send(key).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let mut send_open = true;
    ui.system("interactive session ready; use /help for commands".to_string());
    ui.system(format!(
        "auto-accept is {} for files and clipboard",
        if ui.auto_accept_enabled { "on" } else { "off" }
    ));
    ui.render();

    let shutdown_signal = wait_for_shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            maybe_key = input_rx.recv() => {
                let Some(key) = maybe_key else {
                    if send_open {
                        shutdown_interactive_session(
                            session_id,
                            send,
                            ui,
                            "input closed",
                            &mut registry,
                        ).await?;
                    }
                    break;
                };

                if !send_open {
                    continue;
                }

                if let Some(line) = ui.handle_key_event(key) {
                    send_open = handle_user_input(
                        parse_user_input(&line),
                        session_id,
                        connection,
                        send,
                        &config.download_dir,
                        ui,
                        &task_tx,
                        &mut registry,
                    )
                    .await?;
                }
                ui.render();
            }
            envelope = read_envelope(recv) => {
                match envelope? {
                    Some(envelope) => {
                        handle_interactive_envelope(
                            session_id,
                            connection,
                            send,
                            &config.download_dir,
                            ui,
                            &task_tx,
                            &mut registry,
                            envelope,
                        ).await?;
                    }
                    None => break,
                }
                ui.render();
            }
            maybe_event = task_rx.recv() => {
                let Some(event) = maybe_event else { break; };
                handle_transfer_task_event(session_id, send, ui, &mut registry, event).await?;
                ui.render();
            }
            _ = &mut shutdown_signal => {
                if send_open {
                    shutdown_interactive_session(
                        session_id,
                        send,
                        ui,
                        "terminal disconnected",
                        &mut registry,
                    ).await?;
                    ui.render();
                }
                break;
            }
        }
    }

    Ok(())
}
