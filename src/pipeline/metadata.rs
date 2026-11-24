use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TofChannel {
    Depth,
    Nir,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TofEmbedded {
    pub capture_sequence: u32,
    pub sensor_frame_count: u16,
    pub sensor_clock: TofSensorClock,
    pub frame_flag: u8,
    pub dummy_byte: u8,
    pub padding_byte: u8,
    pub reserved_bytes: [u8; 16],
    pub undocumented_bytes: [u8; 32],
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TofSensorClock {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub microsecond: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TofPairMetadata {
    /// 单通道传感器（只出 depth 或只出 nir）时，缺失的一侧为 None 并补黑。
    pub depth: Option<TofEmbedded>,
    pub nir: Option<TofEmbedded>,
}

#[derive(Default)]
pub(super) enum FrameMetadata {
    #[default]
    None,
    TofSample(TofChannel, TofEmbedded),
    TofPair(TofPairMetadata),
}
