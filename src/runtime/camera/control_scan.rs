use super::Camera;
use crate::{
    runtime::{ControlRead, Failure, Responder, serialize},
    v4l::{ControlGroup, ControlScan, ControlValueScan, ReadableControl},
};
use serde_json::json;
use std::time::{Duration, Instant};

const SLOW_CONTROL_THRESHOLD: Duration = Duration::from_millis(10);
// At 60 raw fps the ToF pipeline needs consecutive captures to output a pair.
const CAPTURE_RECOVERY_DELAY: Duration = Duration::from_millis(50);

enum ControlScanKind {
    List(ControlGroup, ControlScan),
    Values(ControlValueScan),
}

pub(super) struct PendingControlScan {
    selection_id: u64,
    scan: ControlScanKind,
    sender: Responder,
    resume_at: Instant,
}

impl Camera {
    pub(super) fn start_control_scan(&mut self, read: ControlRead, sender: Responder) {
        if let ControlRead::Values(target) = &read
            && let Err(error) = self.require_target(target)
        {
            let _ = sender.send(Err(error));
            return;
        }
        if self
            .pending_control_scan
            .as_ref()
            .is_some_and(|pending| !pending.sender.is_closed())
        {
            let _ = sender.send(Err(Failure::Unavailable));
            return;
        }
        self.pending_control_scan = None;
        let Some(selection) = &self.selection else {
            let _ = sender.send(Ok(json!([])));
            return;
        };
        let selection_id = selection.snapshot.selection_id;
        let scan = match read {
            ControlRead::List(group) => selection
                .device
                .scan_controls(group)
                .map(|scan| ControlScanKind::List(group, scan)),
            ControlRead::Values(_) => Ok(ControlScanKind::Values(ControlValueScan::new(
                selection.readable_controls.values().copied().collect(),
            ))),
        };
        match self.track_device_error(scan) {
            Ok(scan) => {
                self.pending_control_scan = Some(PendingControlScan {
                    selection_id,
                    scan,
                    sender,
                    resume_at: Instant::now(),
                })
            }
            Err(error) => {
                let _ = sender.send(Err(error.into()));
            }
        }
    }

    pub(super) fn step_control_scan(&mut self) {
        let Some(mut pending) = self.pending_control_scan.take() else {
            return;
        };
        if pending.sender.is_closed() {
            return;
        }
        let Some(selection) = self
            .selection
            .as_ref()
            .filter(|selected| selected.snapshot.selection_id == pending.selection_id)
        else {
            let _ = pending.sender.send(Err(Failure::Stale));
            return;
        };
        let started_at = Instant::now();
        if started_at < pending.resume_at {
            self.pending_control_scan = Some(pending);
            return;
        }
        let step = match &mut pending.scan {
            ControlScanKind::List(group, scan) => {
                let result = scan.step(&selection.device);
                self.track_device_error(result).map(|completed| {
                    completed.map(|controls| {
                        let readable = controls.iter().filter_map(|control| control.readable());
                        self.refresh_readable_controls(*group, readable);
                        serialize(controls)
                    })
                })
            }
            ControlScanKind::Values(scan) => {
                let result = scan.step(&selection.device);
                self.track_device_error(result)
                    .map(|completed| completed.map(serialize))
            }
        };
        match step {
            Ok(None) => {
                let finished_at = Instant::now();
                if self.session.is_some()
                    && finished_at.duration_since(started_at) >= SLOW_CONTROL_THRESHOLD
                {
                    pending.resume_at = finished_at + CAPTURE_RECOVERY_DELAY;
                }
                self.pending_control_scan = Some(pending);
            }
            Ok(Some(reply)) => {
                let _ = pending.sender.send(reply);
            }
            Err(error) => {
                let _ = pending.sender.send(Err(error.into()));
            }
        }
    }

    fn refresh_readable_controls(
        &mut self,
        group: ControlGroup,
        readable: impl Iterator<Item = ReadableControl>,
    ) {
        let Some(selection) = &mut self.selection else {
            return;
        };
        selection
            .readable_controls
            .retain(|id, _| !group.includes(*id));
        selection
            .readable_controls
            .extend(readable.map(|control| (control.id, control)));
    }

    pub(super) fn cancel_control_scan(&mut self) {
        if let Some(pending) = self.pending_control_scan.take() {
            let _ = pending.sender.send(Err(Failure::Stale));
        }
    }
}
