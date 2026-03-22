use std::time::{Duration, Instant};

use skyffla_protocol::room::TransferPhase;

const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub(crate) struct ProgressEmissionGate {
    last_phase: Option<TransferPhase>,
    last_emit_at: Option<Instant>,
    seen_any: bool,
}

impl Default for ProgressEmissionGate {
    fn default() -> Self {
        Self {
            last_phase: None,
            last_emit_at: None,
            seen_any: false,
        }
    }
}

impl ProgressEmissionGate {
    pub(crate) fn should_emit(
        &mut self,
        phase: &TransferPhase,
        bytes_complete: u64,
        bytes_total: Option<u64>,
    ) -> bool {
        let phase_changed = self.last_phase.as_ref() != Some(phase);
        let is_final = bytes_total.is_some_and(|total| bytes_complete >= total);
        let now = Instant::now();
        let interval_elapsed = self
            .last_emit_at
            .is_none_or(|last_emit_at| now.duration_since(last_emit_at) >= PROGRESS_EMIT_INTERVAL);

        let should_emit = !self.seen_any || phase_changed || is_final || interval_elapsed;
        if should_emit {
            self.seen_any = true;
            self.last_phase = Some(phase.clone());
            self.last_emit_at = Some(now);
        }
        should_emit
    }
}
