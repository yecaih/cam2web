use super::Camera;
use crate::{
    config::validate_format,
    pipeline::Pipeline,
    runtime::{
        Output,
        diagnostics::{Diagnostics, unix_ms},
        session::{Session, lock},
    },
    v4l::{CaptureEvent, FormatRequest},
};
use anyhow::{Context, Result};
use std::sync::Arc;

const CAPTURE_POLL_TIMEOUT_MS: i32 = 20;

impl Camera {
    pub(super) fn stop_preview(&mut self) -> Result<()> {
        self.cancel_control_scan();
        self.preview_publisher.send_replace(Output::default());
        self.frame_slot.clear();
        if let Some(session) = self.session.take() {
            self.last_diagnostics = session.snapshot();
        }
        if let Some(selection) = &mut self.selection
            && let Err(error) = selection.device.stop()
        {
            self.selection = None;
            return Err(error).context("stop preview; device selection revoked");
        }
        Ok(())
    }

    pub(super) fn start_preview(&mut self, request: Option<FormatRequest>) -> Result<()> {
        let selection = self.selection.as_ref().context("no device selected")?;
        anyhow::ensure!(
            selection.snapshot.capabilities.can_stream,
            "node has no supported capture buffer type or streaming I/O"
        );
        let profile = self
            .inventory
            .device_profile(selection.identity)?
            .context("no device profile for this camera name; preview disabled")?;
        anyhow::ensure!(
            selection.profile.as_ref().map(|profile| &profile.name) == Some(&profile.name),
            "camera profile changed; select the node again"
        );
        let requested = request.unwrap_or(profile.capture);
        validate_format(&requested)?;
        Pipeline::validate(&profile.pipeline, &requested.fourcc)?;
        self.stop_preview()?;
        self.last_diagnostics = Diagnostics::default();
        let device = &mut self
            .selection
            .as_mut()
            .context("device selection lost")?
            .device;
        let result = device.start(&requested).context("start V4L2 preview");
        let negotiated = self.track_device_error(result).and_then(|spec| {
            Pipeline::build(&profile.pipeline, &spec).map(|pipeline| (spec, pipeline))
        });
        let (spec, mut pipeline) = match negotiated {
            Ok(negotiated) => negotiated,
            Err(error) => {
                return match self.stop_preview() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!("cleanup also failed: {cleanup:#}"))),
                };
            }
        };
        self.last_error = None;
        self.restore_parameter_overrides(&profile.name, &mut pipeline);
        let session_id = self.issue_id();
        self.preview_publisher.send_replace(Output {
            session_id: Some(session_id),
            ..Output::default()
        });
        self.session = Some(Arc::new(Session::new(
            session_id, requested, spec, pipeline,
        )));
        Ok(())
    }

    fn restore_parameter_overrides(&mut self, profile_name: &str, pipeline: &mut Pipeline) {
        if let Some(overrides) = self.parameter_overrides.get_mut(profile_name) {
            let mut reset = Vec::new();
            overrides.retain(|(instance, parameter), value| {
                if let Err(error) = pipeline.set_parameter(instance, parameter, value.clone()) {
                    let actual = pipeline
                        .parameters()
                        .into_iter()
                        .find(|entry| {
                            entry.instance_id == *instance && entry.parameter_id == *parameter
                        })
                        .map(|entry| entry.value);
                    reset.push(format!(
                        "{instance}/{parameter} reset to {actual:?}: {error:#}"
                    ));
                    return false;
                }
                true
            });
            if !reset.is_empty() {
                self.last_error = Some(reset.join("; "));
            }
        }
    }

    pub(super) fn poll_frame(&mut self) {
        let (Some(session), Some(selection)) = (&self.session, &mut self.selection) else {
            return;
        };
        match selection.device.capture(CAPTURE_POLL_TIMEOUT_MS) {
            Ok(CaptureEvent::Frame(frame)) => {
                let mut diagnostics = lock(&session.diagnostics);
                diagnostics.captured_count += 1;
                diagnostics.last_capture_unix_ms = Some(unix_ms());
                diagnostics.last_sequence = Some(frame.sequence);
                drop(diagnostics);
                self.frame_slot.replace(session, frame);
            }
            Ok(CaptureEvent::Timeout) => {}
            Ok(CaptureEvent::DriverError { sequence }) => {
                Self::record_invalid_buffer(session, sequence, "V4L2_BUF_FLAG_ERROR");
            }
            Ok(CaptureEvent::Empty { sequence }) => {
                Self::record_invalid_buffer(session, sequence, "zero bytes used");
            }
            Err(error) => {
                let reason = format!("capture failed: {error:#}");
                lock(&session.diagnostics).last_capture_error = Some(reason.clone());
                if self.track_device_error(Err::<(), _>(error)).is_err() && self.session.is_some() {
                    let cleanup = self.stop_preview();
                    self.last_error = Some(match cleanup {
                        Ok(()) => reason,
                        Err(error) => format!("{reason}; additionally {error:#}"),
                    });
                }
            }
        }
    }

    fn record_invalid_buffer(session: &Session, sequence: u32, reason: &str) {
        let mut diagnostics = lock(&session.diagnostics);
        diagnostics.invalid_capture_count += 1;
        diagnostics.last_capture_error =
            Some(format!("discarded capture buffer {sequence}: {reason}"));
    }
}
