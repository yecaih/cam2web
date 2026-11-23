use super::diagnostics::Diagnostics;
use crate::{
    pipeline::Pipeline,
    v4l::{CaptureSpec, FormatRequest},
};
use serde::Serialize;
use std::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

#[derive(Serialize)]
pub(super) struct PreviewSnapshot {
    pub(super) session_id: u64,
    requested: FormatRequest,
    spec: CaptureSpec,
}

pub(super) struct Session {
    pub(super) preview: PreviewSnapshot,
    started_at: Instant,
    pub(super) pipeline: Mutex<Pipeline>,
    pub(super) diagnostics: Mutex<Diagnostics>,
}

impl Session {
    pub(super) fn new(
        session_id: u64,
        requested: FormatRequest,
        spec: CaptureSpec,
        pipeline: Pipeline,
    ) -> Self {
        Self {
            preview: PreviewSnapshot {
                session_id,
                requested,
                spec,
            },
            started_at: Instant::now(),
            pipeline: Mutex::new(pipeline),
            diagnostics: Mutex::default(),
        }
    }

    pub(super) fn snapshot(&self) -> Diagnostics {
        let mut diagnostics = lock(&self.diagnostics).clone();
        let elapsed_seconds = self.started_at.elapsed().as_secs_f64().max(0.001);
        diagnostics.capture_fps = diagnostics.captured_count as f64 / elapsed_seconds;
        diagnostics.output_fps = diagnostics.output_count as f64 / elapsed_seconds;
        diagnostics
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
