use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::Result;
use iroh::endpoint::SendStream;
use skyffla_protocol::{ControlMessage, Envelope, Reject};
use skyffla_transport::IrohConnection;
use tokio::sync::mpsc;

use crate::accept_policy::AutoAcceptPolicy;
use crate::local_state::update_local_state;
use crate::net::framing::{next_message_id, send_transfer_error, write_envelope};
use crate::runtime::handshake::send_chat_message;
use crate::runtime::interactive_control::{
    cancel_transfer, shutdown_interactive_session, TransferRegistry,
};
use crate::transfers::{accept_pending_offer, send_clipboard, send_path, TransferTaskEvent};
use crate::ui::{help_lines, resolve_cancel_target, UiState, UserInput};

pub(crate) async fn handle_user_input(
    input: UserInput,
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    download_dir: &Path,
    ui: &mut UiState,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
    registry: &mut TransferRegistry,
) -> Result<bool> {
    match input {
        UserInput::Quit => {
            shutdown_interactive_session(session_id, send, ui, "user requested shutdown", registry)
                .await?;
            Ok(false)
        }
        UserInput::Chat(text) => {
            if !text.is_empty() {
                send_chat_message(session_id, send, &text, Some(ui), None).await?;
            }
            Ok(true)
        }
        UserInput::SendFile(path) => {
            match send_path(session_id, send, &path, ui).await {
                Ok(plan) => {
                    registry
                        .pending_outgoing
                        .insert(plan.offer.transfer_id.clone(), plan);
                }
                Err(error) => ui.system(format!("send failed: {error:#}")),
            }
            Ok(true)
        }
        UserInput::SendClipboard => {
            match send_clipboard(session_id, send, ui).await {
                Ok(plan) => {
                    registry
                        .pending_outgoing
                        .insert(plan.offer.transfer_id.clone(), plan);
                }
                Err(error) => ui.system(format!("clipboard send failed: {error:#}")),
            }
            Ok(true)
        }
        UserInput::Accept => {
            if let Some(offer) = ui.pending_offer.clone() {
                ui.pending_offer = None;
                match accept_pending_offer(
                    session_id,
                    connection,
                    send,
                    download_dir,
                    offer.clone(),
                    ui,
                    task_tx.clone(),
                )
                .await
                {
                    Ok(cancel) => {
                        registry
                            .running_transfers
                            .insert(offer.transfer_id.clone(), cancel);
                    }
                    Err(error) => {
                        let _ = send_transfer_error(
                            session_id,
                            send,
                            "transfer_failed",
                            &error.to_string(),
                            Some(offer.transfer_id.as_str()),
                        )
                        .await;
                        ui.mark_transfer_cancelled(&offer.transfer_id);
                        ui.system(format!("receive failed for {}: {error:#}", offer.name));
                    }
                }
            } else {
                ui.system("no pending offer to accept".to_string());
            }
            Ok(true)
        }
        UserInput::Reject => {
            if let Some(offer) = ui.pending_offer.take() {
                write_envelope(
                    send,
                    &Envelope::new(
                        session_id,
                        next_message_id(),
                        ControlMessage::Reject(Reject {
                            transfer_id: offer.transfer_id.clone(),
                            reason: Some("user rejected".to_string()),
                        }),
                    ),
                )
                .await?;
                ui.mark_transfer_rejected(&offer.transfer_id);
                ui.system(format!("rejected file {}", offer.name));
            } else {
                ui.system("no pending offer to reject".to_string());
            }
            Ok(true)
        }
        UserInput::Cancel(requested) => {
            match resolve_cancel_target(ui, requested.as_deref()) {
                Ok(Some(transfer_id)) => {
                    cancel_transfer(session_id, send, ui, &transfer_id, "user cancelled").await?;
                    if let Some(cancel) = registry.running_transfers.remove(&transfer_id) {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    registry.pending_outgoing.remove(&transfer_id);
                }
                Ok(None) => ui.system("no active transfer to cancel".to_string()),
                Err(message) => ui.system(message),
            }
            Ok(true)
        }
        UserInput::AutoAccept(enabled) => {
            match enabled {
                Some(enabled) => {
                    ui.auto_accept_policy = if enabled {
                        AutoAcceptPolicy::files_and_clipboard()
                    } else {
                        AutoAcceptPolicy::none()
                    };
                    ui.auto_accept_source = "interactive override".to_string();
                    if let Err(error) = update_local_state(&ui.state_path, |state| {
                        state.auto_accept_policy = ui.auto_accept_policy.clone();
                    }) {
                        ui.system(format!(
                            "failed to persist auto-accept preference: {error:#}"
                        ));
                    } else {
                        ui.system(format!(
                            "persisted auto-accept default {} for file and clipboard offers",
                            if enabled { "enabled" } else { "disabled" }
                        ));
                    }
                }
                None => {
                    ui.system(ui.auto_accept_status_line());
                }
            }
            Ok(true)
        }
        UserInput::Help => {
            for line in help_lines() {
                ui.system(line.to_string());
            }
            Ok(true)
        }
    }
}
