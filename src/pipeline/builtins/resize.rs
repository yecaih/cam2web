use anyhow::{Context, Result};
use serde::Deserialize;

use super::super::{
    frame::{Frame, Layout, PixelFormat, allocate_plane, ensure_rgb},
    module::Module,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::pipeline) struct ResizeNearest {
    width: u32,
    height: u32,
}

impl Module for ResizeNearest {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat> {
        ensure_rgb(input)?;
        Layout::packed_rgb(self.width, self.height).context("invalid resize dimensions")?;
        Ok(PixelFormat::Rgb3)
    }

    fn output_layout(&self, _input: Layout) -> Result<Layout> {
        Layout::packed_rgb(self.width, self.height)
    }

    fn process(&mut self, frame: Frame, output: Layout) -> Result<Option<Frame>> {
        if frame.layout.width == output.width
            && frame.layout.height == output.height
            && frame.layout.stride_bytes == output.stride_bytes
        {
            return Ok(Some(frame));
        }
        let mut rgb = allocate_plane(output)?;
        let src_row_bytes = frame.layout.format.row_bytes(frame.layout.width)? as usize;
        for row_index in 0..output.height {
            let src_row = nearest_index(row_index, frame.layout.height, output.height);
            let row = frame.row(src_row, src_row_bytes);
            append_resized_row(&mut rgb, row, frame.layout.width, output.width);
        }
        Ok(Some(frame.into_with_plane(output, rgb)))
    }
}

fn nearest_index(index: u32, src_extent: u32, dst_extent: u32) -> u32 {
    (u64::from(index) * u64::from(src_extent) / u64::from(dst_extent)) as u32
}

fn append_resized_row(rgb: &mut Vec<u8>, row: &[u8], src_width: u32, dst_width: u32) {
    for col_index in 0..dst_width {
        let src_offset = nearest_index(col_index, src_width, dst_width) as usize * 3;
        rgb.extend_from_slice(&row[src_offset..src_offset + 3]);
    }
}
