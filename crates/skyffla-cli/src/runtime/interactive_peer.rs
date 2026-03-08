use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{bail, Result};
use iroh::endpoint::SendStream;
use skyffla_protocol::{Complete, ControlMessage, Envelope, TransferKind};
use skyffla_transport::IrohConnection;
use tokio::sync::mpsc;

use crate::net::framing::{next_message_id, send_transfer_error, write_envelope};
use crate::runtime::interactive_control::TransferRegistry;
use crate::transfers::{
    accept_pending_offer, describe_offer_size, format_digest_suffix, spawn_outgoing_transfer_task,
    transfer_kind_label, TransferTaskEvent,
};
use crate::ui::{TransferStateUi, TransferUi, UiState};

pub(crate) async fn handle_interactive_envelope(
    session_id: &str,
    connection: &IrohConnection,
    send: &mut SendStream,
    download_dir: &Path,
    ui: &mut UiState,
    task_tx: &mpsc::UnboundedSender<TransferTaskEvent>,
    registry: &mut TransferRegistry,
    envelope: Envelope,
) -> Result<()> {
    match envelope.payload {
        ControlMessage::ChatMessage(message) => {
            let text = message.text;
            let speaker = ui.peer_name.clone();
            ui.chat(&speaker, &text);
            Ok(())
        }
        ControlMessage::Accept(accept) => {
            let transfer_id = accept.transfer_id;
            if let Some(plan) = registry.pending_outgoing.remove(&transfer_id) {
                ui.mark_transfer_streaming(&transfer_id);
                registry
                    .running_transfers
                    .insert(transfer_id.clone(), plan.cancel.clone());
                spawn_outgoing_transfer_task(connection.clone(), plan, task_tx.clone());
            } else {
                ui.system(format!("transfer {} accepted", transfer_id));
            }
            Ok(())
        }
        ControlMessage::Offer(offer)
            if matches!(
                offer.kind,
                TransferKind::File | TransferKind::FolderArchive | TransferKind::Clipboard
            ) =>
        {
            ui.upsert_transfer(TransferUi {
                id: offer.transfer_id.clone(),
                state: TransferStateUi::AwaitingDecision,
                bytes_done: 0,
                bytes_total: offer.size,
            });
            let should_auto_accept = ui.auto_accept_policy.allows_kind(&offer.kind);
            if should_auto_accept {
                ui.system(format!(
                    "auto-accepting {} {}{}",
                    transfer_kind_label(&offer.kind),
                    offer.name,
                    describe_offer_size(&offer)
                ));
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
                        ui.system(format!("auto-accept failed for {}: {error:#}", offer.name));
                    }
                }
            } else {
                ui.pending_offer = Some(offer.clone());
                ui.system(format!(
                    "incoming {} offer: {}{}. /accept or /reject",
                    transfer_kind_label(&offer.kind),
                    offer.name,
                    describe_offer_size(&offer)
                ));
            }
            Ok(())
        }
        ControlMessage::Complete(complete) => {
            registry.pending_outgoing.remove(&complete.transfer_id);
            registry.running_transfers.remove(&complete.transfer_id);
            ui.mark_transfer_completed(&complete.transfer_id);
            ui.system(format!(
                "transfer {} complete{}",
                complete.transfer_id,
                format_digest_suffix(complete.digest.as_ref())
            ));
            Ok(())
        }
        ControlMessage::Reject(reject) => {
            registry.pending_outgoing.remove(&reject.transfer_id);
            if let Some(cancel) = registry.running_transfers.remove(&reject.transfer_id) {
                cancel.store(true, Ordering::SeqCst);
            }
            ui.mark_transfer_rejected(&reject.transfer_id);
            ui.system(format!(
                "transfer {} rejected{}",
                reject.transfer_id,
                reject
                    .reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        ControlMessage::Cancel(cancel) => {
            registry.pending_outgoing.remove(&cancel.transfer_id);
            if let Some(flag) = registry.running_transfers.remove(&cancel.transfer_id) {
                flag.store(true, Ordering::SeqCst);
            }
            ui.mark_transfer_cancelled(&cancel.transfer_id);
            ui.system(format!(
                "transfer {} cancelled{}",
                cancel.transfer_id,
                cancel
                    .reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        ControlMessage::Error(error) => {
            if let Some(transfer_id) = error.transfer_id.as_deref() {
                registry.pending_outgoing.remove(transfer_id);
                if let Some(flag) = registry.running_transfers.remove(transfer_id) {
                    flag.store(true, Ordering::SeqCst);
                }
                ui.mark_transfer_cancelled(transfer_id);
                ui.system(format!(
                    "transfer {} error {}: {}",
                    transfer_id, error.code, error.message
                ));
            } else {
                ui.system(format!("peer error {}: {}", error.code, error.message));
            }
            Ok(())
        }
        other => bail!("unexpected control message after handshake: {:?}", other),
    }
}

pub(crate) async fn handle_transfer_task_event(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    registry: &mut TransferRegistry,
    event: TransferTaskEvent,
) -> Result<()> {
    match event {
        TransferTaskEvent::Progress {
            transfer_id,
            bytes_done,
            bytes_total,
        } => {
            ui.update_transfer_progress(&transfer_id, bytes_done, bytes_total);
        }
        TransferTaskEvent::LocalComplete {
            transfer_id,
            message,
        } => {
            registry.running_transfers.remove(&transfer_id);
            ui.mark_transfer_completed(&transfer_id);
            ui.system(message);
        }
        TransferTaskEvent::ReceiveComplete {
            transfer_id,
            message,
            digest,
        } => {
            registry.running_transfers.remove(&transfer_id);
            write_envelope(
                send,
                &Envelope::new(
                    session_id,
                    next_message_id(),
                    ControlMessage::Complete(Complete {
                        transfer_id: transfer_id.clone(),
                        digest: Some(digest),
                    }),
                ),
            )
            .await?;
            ui.mark_transfer_completed(&transfer_id);
            ui.system(message);
        }
        TransferTaskEvent::Failed {
            transfer_id,
            message,
            notify_peer,
        } => {
            registry.running_transfers.remove(&transfer_id);
            ui.mark_transfer_cancelled(&transfer_id);
            ui.system(message.clone());
            if notify_peer {
                let _ = send_transfer_error(
                    session_id,
                    send,
                    "transfer_failed",
                    &message,
                    Some(transfer_id.as_str()),
                )
                .await;
            }
        }
    }
    Ok(())
}
