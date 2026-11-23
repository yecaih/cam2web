use super::{
    Output,
    diagnostics::unix_ms,
    session::{Session, lock},
};
use crate::v4l::CapturedFrame;
use axum::body::Bytes;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};
use tokio::sync::watch;

struct PendingFrame {
    session: Arc<Session>,
    capture: CapturedFrame,
}

#[derive(Default)]
struct SlotState {
    latest: Option<PendingFrame>,
    is_closed: bool,
}

#[derive(Default)]
pub(super) struct LatestFrameSlot {
    state: Mutex<SlotState>,
    ready: Condvar,
}

impl LatestFrameSlot {
    pub(super) fn replace(&self, session: &Arc<Session>, capture: CapturedFrame) {
        let mut state = lock(&self.state);
        let pending_frame = PendingFrame {
            session: Arc::clone(session),
            capture,
        };
        if state.latest.replace(pending_frame).is_some() {
            lock(&session.diagnostics).dropped_count += 1;
        }
        self.ready.notify_one();
    }

    fn wait_for_frame(&self) -> Option<PendingFrame> {
        let mut state = self
            .ready
            .wait_while(lock(&self.state), |state| {
                !state.is_closed && state.latest.is_none()
            })
            .unwrap_or_else(|error| error.into_inner());
        state.latest.take()
    }

    pub(super) fn clear(&self) {
        lock(&self.state).latest = None;
    }

    pub(super) fn close(&self) {
        let mut state = lock(&self.state);
        state.latest = None;
        state.is_closed = true;
        self.ready.notify_one();
    }
}

pub(super) fn run_processor(frame_slot: &LatestFrameSlot, publisher: &watch::Sender<Output>) {
    while let Some(PendingFrame { session, capture }) = frame_slot.wait_for_frame() {
        let session_id = session.preview.session_id;
        let start = Instant::now();
        let result = lock(&session.pipeline).process(session_id, capture);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut diagnostics = lock(&session.diagnostics);
        diagnostics.last_processing_ms = Some(elapsed_ms);
        match result {
            Ok(Some(processed)) => {
                diagnostics.last_encode_ms = Some(processed.encode_duration.as_secs_f64() * 1000.0);
                let has_published = publisher.send_if_modified(|output| {
                    if output.session_id != Some(session_id) {
                        return false;
                    }
                    output.jpeg = Some(Bytes::from(processed.jpeg));
                    output.tof_metadata = processed.tof_metadata;
                    true
                });
                if has_published {
                    diagnostics.output_count += 1;
                    diagnostics.last_output_unix_ms = Some(unix_ms());
                }
            }
            Ok(None) => (),
            Err(error) => {
                diagnostics.processing_error_count += 1;
                diagnostics.last_processing_error = Some(format!("{error:#}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pipeline::Pipeline,
        v4l::{CaptureSpec, FormatRequest},
    };

    fn session() -> Arc<Session> {
        let requested = FormatRequest {
            fourcc: "RGB3".into(),
            width: 1,
            height: 1,
            interval: None,
        };
        let spec = CaptureSpec {
            fourcc: requested.fourcc.clone(),
            width: requested.width,
            height: requested.height,
            stride_bytes: 3,
            size_bytes: 3,
            interval: None,
        };
        let config = toml::from_str("[output]\nid = 'jpeg'\nkind = 'jpeg_encode'").unwrap();
        let pipeline = Pipeline::build(&config, &spec).unwrap();
        Arc::new(Session::new(1, requested, spec, pipeline))
    }

    fn capture(sequence: u32) -> CapturedFrame {
        CapturedFrame {
            bytes: vec![0; 3],
            sequence,
        }
    }

    #[test]
    fn replaces_unprocessed_frame_and_counts_the_drop() {
        let slot = LatestFrameSlot::default();
        let session = session();
        slot.replace(&session, capture(10));
        slot.replace(&session, capture(11));
        assert_eq!(session.snapshot().dropped_count, 1);
        let pending = slot.wait_for_frame().unwrap();
        assert_eq!(pending.capture.sequence, 11);
        assert!(Arc::ptr_eq(&pending.session, &session));
    }

    #[test]
    fn clear_discards_pending_frame_without_closing_the_slot() {
        let slot = LatestFrameSlot::default();
        let session = session();
        slot.replace(&session, capture(10));
        slot.clear();
        slot.replace(&session, capture(11));
        assert_eq!(session.snapshot().dropped_count, 0);
        assert_eq!(slot.wait_for_frame().unwrap().capture.sequence, 11);
    }

    #[test]
    fn close_discards_pending_frame_and_ends_processing() {
        let slot = LatestFrameSlot::default();
        slot.replace(&session(), capture(10));
        slot.close();
        assert!(slot.wait_for_frame().is_none());
    }
}
