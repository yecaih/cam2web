use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default, Clone, Serialize)]
pub(super) struct Diagnostics {
    pub(super) captured_count: u64,
    pub(super) output_count: u64,
    pub(super) dropped_count: u64,
    pub(super) invalid_capture_count: u64,
    pub(super) processing_error_count: u64,
    pub(super) last_capture_unix_ms: Option<u64>,
    pub(super) last_output_unix_ms: Option<u64>,
    pub(super) last_processing_ms: Option<f64>,
    pub(super) last_encode_ms: Option<f64>,
    pub(super) capture_fps: f64,
    pub(super) output_fps: f64,
    pub(super) last_sequence: Option<u32>,
    pub(super) last_processing_error: Option<String>,
    pub(super) last_capture_error: Option<String>,
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
