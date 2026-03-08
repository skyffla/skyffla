use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::endpoint::SendStream;

use crate::net::framing::{next_message_id, write_envelope};
use crate::transfers::PendingOutgoingTransfer;
use crate::ui::UiState;

pub(crate) struct TransferRegistry {
    pub(crate) pending_outgoing: HashMap<String, PendingOutgoingTransfer>,
    pub(crate) running_transfers: HashMap<String, Arc<AtomicBool>>,
}

impl TransferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            pending_outgoing: HashMap::new(),
            running_transfers: HashMap::new(),
        }
    }
}

pub(crate) async fn shutdown_interactive_session(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    reason: &str,
    registry: &mut TransferRegistry,
) -> Result<()> {
    let cancelled = cancel_active_transfers(session_id, send, ui, reason, registry).await?;
    if cancelled > 0 {
        ui.system(format!(
            "cancelled {} transfer(s) before closing",
            cancelled
        ));
    }
    send.finish()
        .context("failed to finish control stream send side")?;
    ui.system("closing session".to_string());
    Ok(())
}

#[cfg(unix)]
pub(crate) async fn wait_for_shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("failed to register SIGHUP handler");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sighup.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn wait_for_shutdown_signal() {
    std::future::pending::<()>().await;
}

async fn cancel_active_transfers(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    reason: &str,
    registry: &mut TransferRegistry,
) -> Result<usize> {
    let transfer_ids = ui.cancellable_transfer_ids();
    for transfer_id in &transfer_ids {
        cancel_transfer(session_id, send, ui, transfer_id, reason).await?;
        registry.pending_outgoing.remove(transfer_id);
        if let Some(cancel) = registry.running_transfers.remove(transfer_id) {
            cancel.store(true, Ordering::SeqCst);
        }
    }
    Ok(transfer_ids.len())
}

pub(crate) async fn cancel_transfer(
    session_id: &str,
    send: &mut SendStream,
    ui: &mut UiState,
    transfer_id: &str,
    reason: &str,
) -> Result<()> {
    write_envelope(
        send,
        &skyffla_protocol::Envelope::new(
            session_id,
            next_message_id(),
            skyffla_protocol::ControlMessage::Cancel(skyffla_protocol::Cancel {
                transfer_id: transfer_id.to_string(),
                reason: Some(reason.to_string()),
            }),
        ),
    )
    .await?;
    ui.mark_transfer_cancelled(transfer_id);
    if ui
        .pending_offer
        .as_ref()
        .is_some_and(|offer| offer.transfer_id == transfer_id)
    {
        ui.pending_offer = None;
    }
    ui.system(format!("transfer {} cancelled ({})", transfer_id, reason));
    Ok(())
}
