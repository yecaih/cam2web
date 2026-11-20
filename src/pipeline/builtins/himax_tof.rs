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

struct Half {
    session_id: u64,
    channel: TofChannel,
    embedded: TofEmbedded,
    pixels: Vec<u8>,
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
        let previous = self.pending.take();
        let FrameMetadata::TofSample(channel, embedded) = frame.metadata else {
            bail!("missing parsed Himax ToF metadata");
        };
        let current = Half {
            session_id: frame.identity.session_id,
            channel,
            embedded,
            pixels: self.render(&frame, channel),
        };
        let Some(previous) = previous.filter(|previous| {
            previous.channel != current.channel
                && previous.session_id == current.session_id
                && previous.embedded.capture_sequence.wrapping_add(1) == embedded.capture_sequence
                && previous.embedded.sensor_frame_count.wrapping_add(1)
                    == embedded.sensor_frame_count
        }) else {
            self.pending = Some(current);
            return Ok(None);
        };
        let (depth, nir) = match current.channel {
            TofChannel::Depth => (current, previous),
            TofChannel::Nir => (previous, current),
        };
        let mut stitched = allocate_plane(output)?;
        for (left, right) in depth
            .pixels
            .chunks_exact(SENSOR_WIDTH as usize)
            .zip(nir.pixels.chunks_exact(SENSOR_WIDTH as usize))
        {
            stitched.extend_from_slice(left);
            stitched.extend_from_slice(right);
        }
        frame.metadata = FrameMetadata::TofPair(TofPairMetadata {
            depth: depth.embedded,
            nir: nir.embedded,
        });
        Ok(Some(frame.into_with_plane(output, stitched)))
    }
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
