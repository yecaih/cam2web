use anyhow::{Result, bail, ensure};
use serde::Deserialize;

use super::{
    super::{
        frame::{Frame, Layout, PixelFormat, allocate_plane},
        metadata::{FrameMetadata, TofChannel, TofEmbedded, TofPairMetadata},
        module::Module,
    },
    himax_tof_metadata::{SENSOR_HEIGHT, SENSOR_WIDTH},
};
const DEPTH_LEVEL_COUNT: usize = 1 << 14;
const NIR_LEVEL_COUNT: usize = 1 << 12;
const HALF_ROW_BYTES: usize = SENSOR_WIDTH as usize;
const BLANK_ROW: [u8; HALF_ROW_BYTES] = [0; HALF_ROW_BYTES];

struct Half {
    session_id: u64,
    channel: TofChannel,
    embedded: TofEmbedded,
    pixels: Vec<u8>,
}

impl Half {
    fn is_pair_with(&self, next: &Self) -> bool {
        self.channel != next.channel
            && self.session_id == next.session_id
            && self.embedded.capture_sequence.wrapping_add(1) == next.embedded.capture_sequence
            && self.embedded.sensor_frame_count.wrapping_add(1) == next.embedded.sensor_frame_count
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::pipeline) struct HimaxTofPair {
    #[serde(skip)]
    pending: Option<Half>,
    #[serde(skip, default = "nir_levels")]
    nir_levels: [u8; NIR_LEVEL_COUNT],
}

impl Module for HimaxTofPair {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat> {
        ensure!(
            input == PixelFormat::HimaxTof16,
            "expected Himax ToF samples from himax_tof_metadata"
        );
        Ok(PixelFormat::Grey)
    }

    fn output_layout(&self, input: Layout) -> Result<Layout> {
        ensure!(
            input.width == SENSOR_WIDTH && input.height == SENSOR_HEIGHT,
            "expected Himax ToF 640×480 samples"
        );
        Ok(Layout {
            format: PixelFormat::Grey,
            width: input.width * 2,
            height: input.height,
            stride_bytes: input.width * 2,
        })
    }

    fn process(&mut self, mut frame: Frame, output: Layout) -> Result<Option<Frame>> {
        let FrameMetadata::TofSample(channel, embedded) = frame.metadata else {
            bail!("missing parsed Himax ToF metadata");
        };
        let current = Half {
            session_id: frame.identity.session_id,
            channel,
            embedded,
            pixels: self.render(&frame, channel),
        };
        let Some(previous) = self.pending.take() else {
            self.pending = Some(current);
            return Ok(None);
        };
        // 传感器可能只出单通道，也可能丢帧；配不上时上一半单独成像，缺失的一侧补黑。
        let (depth, nir) = if previous.is_pair_with(&current) {
            match current.channel {
                TofChannel::Depth => (Some(current), Some(previous)),
                TofChannel::Nir => (Some(previous), Some(current)),
            }
        } else {
            self.pending = Some(current);
            match previous.channel {
                TofChannel::Depth => (Some(previous), None),
                TofChannel::Nir => (None, Some(previous)),
            }
        };
        frame.metadata = FrameMetadata::TofPair(TofPairMetadata {
            depth: depth.as_ref().map(|half| half.embedded),
            nir: nir.as_ref().map(|half| half.embedded),
        });
        let stitched = stitch(output, depth.as_ref(), nir.as_ref())?;
        Ok(Some(frame.into_with_plane(output, stitched)))
    }
}

/// depth 在左、nir 在右；缺失的一侧补黑，输出几何始终是 1280×480。
fn stitch(output: Layout, depth: Option<&Half>, nir: Option<&Half>) -> Result<Vec<u8>> {
    let mut stitched = allocate_plane(output)?;
    for row_index in 0..SENSOR_HEIGHT as usize {
        stitched.extend_from_slice(half_row(depth, row_index));
        stitched.extend_from_slice(half_row(nir, row_index));
    }
    Ok(stitched)
}

fn half_row(half: Option<&Half>, row_index: usize) -> &[u8] {
    half.map_or(&BLANK_ROW, |half| {
        &half.pixels[row_index * HALF_ROW_BYTES..(row_index + 1) * HALF_ROW_BYTES]
    })
}

impl HimaxTofPair {
    fn render(&self, frame: &Frame, channel: TofChannel) -> Vec<u8> {
        match channel {
            TofChannel::Nir => samples(frame, 4)
                .map(|code| self.nir_levels[code])
                .collect(),
            TofChannel::Depth => {
                let mut histogram = vec![0; DEPTH_LEVEL_COUNT];
                for code in samples(frame, 2) {
                    histogram[code] += 1;
                }
                let sample_count = (SENSOR_WIDTH * SENSOR_HEIGHT) as usize;
                let low = percentile(&histogram, sample_count, 1);
                let high = percentile(&histogram, sample_count, 99);
                let scale = 255.0 / (high - low).max(1.0);
                samples(frame, 2)
                    .map(|code| ((code as f64 - low) * scale).clamp(0.0, 255.0) as u8)
                    .collect()
            }
        }
    }
}

fn samples(frame: &Frame, shift: u8) -> impl Iterator<Item = usize> {
    (0..SENSOR_HEIGHT).flat_map(move |row_index| {
        frame
            .row(row_index, SENSOR_WIDTH as usize * 2)
            .chunks_exact(2)
            .map(move |sample| usize::from(u16::from_le_bytes([sample[0], sample[1]]) >> shift))
    })
}

fn percentile(histogram: &[usize], sample_count: usize, percent: usize) -> f64 {
    let rank = (sample_count - 1) * percent;
    let floor_rank = rank / 100;
    let ceil_rank = rank.div_ceil(100);
    let mut cumulative_count = 0;
    let mut floor_code = None;
    for (code, count) in histogram.iter().enumerate() {
        cumulative_count += count;
        if cumulative_count > floor_rank {
            let floor = *floor_code.get_or_insert(code) as f64;
            if cumulative_count > ceil_rank {
                return floor + (code as f64 - floor) * (rank % 100) as f64 / 100.0;
            }
        }
    }
    0.0
}

fn nir_levels() -> [u8; NIR_LEVEL_COUNT] {
    std::array::from_fn(|code| {
        (255.0 * (code as f64).ln_1p() / (NIR_LEVEL_COUNT as f64).ln()) as u8
    })
}
