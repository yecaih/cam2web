use anyhow::{Result, bail, ensure};
use serde::Deserialize;

use super::super::{
    frame::{Frame, Layout, PixelFormat, allocate_plane},
    module::Module,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::pipeline) struct ToRgb {}

impl Module for ToRgb {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat> {
        ensure!(
            matches!(
                input,
                PixelFormat::Yuyv | PixelFormat::Grey | PixelFormat::Rgb3
            ),
            "expected YUYV, GREY or RGB3"
        );
        Ok(PixelFormat::Rgb3)
    }

    fn output_layout(&self, input: Layout) -> Result<Layout> {
        if input.format == PixelFormat::Rgb3 {
            return Ok(input);
        }
        Layout::packed_rgb(input.width, input.height)
    }

    fn process(&mut self, frame: Frame, output: Layout) -> Result<Option<Frame>> {
        if frame.layout.format == PixelFormat::Rgb3 {
            return Ok(Some(frame));
        }
        let row_bytes = frame.layout.format.row_bytes(frame.layout.width)? as usize;
        let mut rgb = allocate_plane(output)?;
        for row_index in 0..frame.layout.height {
            let row = frame.row(row_index, row_bytes);
            match frame.layout.format {
                PixelFormat::Yuyv => append_yuyv_row(&mut rgb, row),
                PixelFormat::Grey => append_grey_row(&mut rgb, row),
                _ => bail!("unexpected input format for to_rgb"),
            }
        }
        Ok(Some(frame.into_with_plane(output, rgb)))
    }
}

fn append_yuyv_row(rgb: &mut Vec<u8>, row: &[u8]) {
    for pair in row.chunks_exact(4) {
        let blue_chroma = i32::from(pair[1]) - 128;
        let red_chroma = i32::from(pair[3]) - 128;
        rgb.extend_from_slice(&yuv601_rgb(pair[0], blue_chroma, red_chroma));
        rgb.extend_from_slice(&yuv601_rgb(pair[2], blue_chroma, red_chroma));
    }
}

fn append_grey_row(rgb: &mut Vec<u8>, row: &[u8]) {
    for &luma in row {
        rgb.extend_from_slice(&[luma; 3]);
    }
}

// CaptureSpec does not carry colorimetry; the MVP assumes limited-range BT.601.
fn yuv601_rgb(luma: u8, blue_chroma: i32, red_chroma: i32) -> [u8; 3] {
    let luma = 298 * (i32::from(luma) - 16);
    let channel = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [
        channel(luma + 409 * red_chroma),
        channel(luma - 100 * blue_chroma - 208 * red_chroma),
        channel(luma + 516 * blue_chroma),
    ]
}
