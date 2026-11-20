use anyhow::{Result, bail, ensure};
use serde::Deserialize;

use super::super::{
    frame::{Frame, Layout, PixelFormat},
    metadata::{FrameMetadata, TofChannel, TofEmbedded, TofSensorClock},
    module::Module,
};

pub(super) const SENSOR_WIDTH: u32 = 640;
pub(super) const SENSOR_HEIGHT: u32 = 480;
const WIRE_ROW_BYTES: u32 = SENSOR_WIDTH * 2;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::pipeline) struct HimaxTofMetadata {}

impl Module for HimaxTofMetadata {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat> {
        ensure!(
            input == PixelFormat::Rggb,
            "expected Orin Himax ToF RGGB transport"
        );
        Ok(PixelFormat::HimaxTof16)
    }

    fn output_layout(&self, input: Layout) -> Result<Layout> {
        ensure!(
            input.width == WIRE_ROW_BYTES && input.height == SENSOR_HEIGHT + 1,
            "Himax ToF requires RGGB 1280×481 (including the embedded line)"
        );
        Ok(Layout {
            format: PixelFormat::HimaxTof16,
            width: SENSOR_WIDTH,
            height: SENSOR_HEIGHT,
            stride_bytes: input.stride_bytes,
        })
    }

    fn process(&mut self, mut frame: Frame, output: Layout) -> Result<Option<Frame>> {
        let embedded = frame.row(SENSOR_HEIGHT, WIRE_ROW_BYTES as usize);
        let channel = match embedded[1] {
            0x00 => TofChannel::Nir,
            0x01 => TofChannel::Depth,
            flag => bail!("unknown Himax ToF embedded frame flag {flag:#04x}"),
        };
        frame.metadata = FrameMetadata::TofSample(
            channel,
            TofEmbedded {
                capture_sequence: frame.identity.sequence,
                sensor_frame_count: u16::from_le_bytes([embedded[2], embedded[3]]),
                sensor_clock: TofSensorClock {
                    day: embedded[4],
                    month: embedded[5],
                    year: u16::from_le_bytes([embedded[6], embedded[7]]),
                    second: embedded[8],
                    minute: embedded[9],
                    hour: embedded[10],
                    microsecond: u32::from_le_bytes(embedded[12..16].try_into()?),
                },
            },
        );
        // The rows have the same byte width before and after unpacking the geometry.
        frame.plane.truncate(output.required_bytes()?);
        frame.layout = output;
        Ok(Some(frame))
    }
}
