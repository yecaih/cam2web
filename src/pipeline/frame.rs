use anyhow::{Context, Result, bail, ensure};

use super::metadata::FrameMetadata;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PixelFormat {
    Yuyv,
    Grey,
    Rgb3,
    Rggb,
    HimaxTof16,
    Jpeg,
}

impl PixelFormat {
    pub(super) fn from_capture(fourcc: &str) -> Result<Self> {
        match fourcc {
            "YUYV" => Ok(Self::Yuyv),
            "GREY" => Ok(Self::Grey),
            "RGB3" => Ok(Self::Rgb3),
            "RGGB" => Ok(Self::Rggb),
            _ => bail!("unsupported capture FourCC {fourcc:?}"),
        }
    }

    pub(super) fn fourcc(self) -> &'static str {
        match self {
            Self::Yuyv => "YUYV",
            Self::Grey => "GREY",
            Self::Rgb3 => "RGB3",
            Self::Rggb => "RGGB",
            Self::HimaxTof16 => "Himax ToF 16-bit LE",
            Self::Jpeg => "JPEG",
        }
    }

    pub(super) fn row_bytes(self, width: u32) -> Result<u32> {
        let bytes_per_pixel = match self {
            Self::Yuyv => {
                ensure!(
                    width.is_multiple_of(2),
                    "YUYV requires even width, got {width}"
                );
                2
            }
            Self::Grey | Self::Rggb => 1,
            Self::HimaxTof16 => 2,
            Self::Rgb3 => 3,
            Self::Jpeg => bail!("JPEG has no fixed row size"),
        };
        width
            .checked_mul(bytes_per_pixel)
            .context("row size overflow")
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub(super) format: PixelFormat,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride_bytes: u32,
}

impl Layout {
    pub(super) fn packed_rgb(width: u32, height: u32) -> Result<Self> {
        let layout = Self {
            format: PixelFormat::Rgb3,
            width,
            height,
            stride_bytes: PixelFormat::Rgb3.row_bytes(width)?,
        };
        layout.required_bytes()?;
        Ok(layout)
    }

    pub(super) fn required_bytes(self) -> Result<usize> {
        ensure!(self.width > 0 && self.height > 0, "zero-sized frame");
        let row_bytes = self.format.row_bytes(self.width)?;
        ensure!(
            self.stride_bytes >= row_bytes,
            "stride {} bytes is smaller than the {}-byte row",
            self.stride_bytes,
            row_bytes
        );
        // bytesused need not include padding after the last row.
        let extent_bytes =
            u64::from(self.height - 1) * u64::from(self.stride_bytes) + u64::from(row_bytes);
        ensure!(extent_bytes <= isize::MAX as u64, "frame extent overflow");
        Ok(extent_bytes as usize)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameIdentity {
    pub(super) session_id: u64,
    pub(super) sequence: u32,
}

pub(super) struct Frame {
    pub(super) identity: FrameIdentity,
    pub(super) layout: Layout,
    pub(super) plane: Vec<u8>,
    pub(super) metadata: FrameMetadata,
}

impl Frame {
    pub(super) fn into_with_plane(self, layout: Layout, plane: Vec<u8>) -> Self {
        Self {
            layout,
            plane,
            ..self
        }
    }

    pub(super) fn row(&self, row_index: u32, row_bytes: usize) -> &[u8] {
        let start = row_index as usize * self.layout.stride_bytes as usize;
        &self.plane[start..start + row_bytes]
    }

    pub(super) fn packed_plane(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        let row_bytes = self.layout.format.row_bytes(self.layout.width)?;
        if self.layout.stride_bytes == row_bytes {
            return Ok(std::borrow::Cow::Borrowed(
                &self.plane[..self.layout.required_bytes()?],
            ));
        }
        let mut packed = allocate_plane(Layout {
            stride_bytes: row_bytes,
            ..self.layout
        })?;
        for row_index in 0..self.layout.height {
            packed.extend_from_slice(self.row(row_index, row_bytes as usize));
        }
        Ok(std::borrow::Cow::Owned(packed))
    }
}

pub(super) fn allocate_plane(layout: Layout) -> Result<Vec<u8>> {
    let plane_bytes = layout.required_bytes()?;
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(plane_bytes)
        .context("allocate output plane")?;
    Ok(plane)
}

pub(super) fn ensure_rgb(input: PixelFormat) -> Result<()> {
    ensure!(input == PixelFormat::Rgb3, "expected RGB3");
    Ok(())
}
