use super::{
    Output,
    diagnostics::unix_ms,
    session::{Session, lock},
};
use crate::v4l::Captured;
use axum::body::Bytes;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};
use tokio::sync::watch;

#[derive(Default)]
struct Pending {
    frame: Option<(Arc<Session>, Captured)>,
    is_closed: bool,
}

#[derive(Default)]
pub(super) struct WorkSlot {
    pending: Mutex<Pending>,
    ready: Condvar,
}

impl WorkSlot {
    pub(super) fn push(&self, session: &Arc<Session>, frame: Captured) {
        let mut pending = lock(&self.pending);
        if pending
            .frame
            .replace((Arc::clone(session), frame))
            .is_some()
        {
            lock(&session.diagnostics).dropped_count += 1;
        }
        self.ready.notify_one();
    }

    fn take(&self) -> Option<(Arc<Session>, Captured)> {
        let mut pending = self
            .ready
            .wait_while(lock(&self.pending), |pending| {
                !pending.is_closed && pending.frame.is_none()
            })
            .unwrap_or_else(|error| error.into_inner());
        pending.frame.take()
    }

    pub(super) fn clear(&self) {
        lock(&self.pending).frame = None;
    }

    pub(super) fn close(&self) {
        let mut pending = lock(&self.pending);
        pending.frame = None;
        pending.is_closed = true;
        self.ready.notify_one();
    }
}

pub(super) fn run_processor(work: &WorkSlot, publisher: &watch::Sender<Output>) {
    while let Some((session, frame)) = work.take() {
        let session_id = session.preview.session_id;
        let start = Instant::now();
        let result = lock(&session.pipeline).process(session_id, frame);
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
