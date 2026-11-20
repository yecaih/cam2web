use anyhow::{Context, Result, ensure};
use image::{ExtendedColorType, codecs::jpeg::JpegEncoder};
use serde::Deserialize;
use serde_json::Value;

use super::super::{
    frame::{Frame, Layout, PixelFormat},
    module::{Module, Parameter},
};

const MIN_QUALITY: u8 = 1;
const MAX_QUALITY: u8 = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::pipeline) struct JpegEncode {
    #[serde(default = "default_quality")]
    quality: u8,
}

fn default_quality() -> u8 {
    80
}

fn validate_quality(quality: u8) -> Result<()> {
    ensure!(
        (MIN_QUALITY..=MAX_QUALITY).contains(&quality),
        "JPEG quality must be an integer in {MIN_QUALITY}..={MAX_QUALITY}"
    );
    Ok(())
}

impl Module for JpegEncode {
    fn output_format(&self, input: PixelFormat) -> Result<PixelFormat> {
        ensure!(
            matches!(input, PixelFormat::Rgb3 | PixelFormat::Grey),
            "JPEG input must be RGB3 or GREY"
        );
        validate_quality(self.quality)?;
        Ok(PixelFormat::Jpeg)
    }

    fn output_layout(&self, input: Layout) -> Result<Layout> {
        ensure!(
            u16::try_from(input.width).is_ok() && u16::try_from(input.height).is_ok(),
            "JPEG dimensions exceed {} pixels per side",
            u16::MAX
        );
        // The encoder validates the packed byte count using u64, but its pixel
        // indexing uses u32.
        ensure!(
            u64::from(input.format.row_bytes(input.width)?) * u64::from(input.height)
                <= u64::from(u32::MAX),
            "packed input exceeds JPEG encoder indexing range"
        );
        Ok(Layout {
            format: PixelFormat::Jpeg,
            stride_bytes: 0,
            ..input
        })
    }

    fn process(&mut self, frame: Frame, output: Layout) -> Result<Option<Frame>> {
        let color = match frame.layout.format {
            PixelFormat::Grey => ExtendedColorType::L8,
            PixelFormat::Rgb3 => ExtendedColorType::Rgb8,
            _ => anyhow::bail!("unexpected JPEG input format"),
        };
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, self.quality).encode(
            &frame.packed_plane()?,
            output.width,
            output.height,
            color,
        )?;
        Ok(Some(frame.into_with_plane(output, jpeg)))
    }

    fn parameters(&self, instance_id: &str) -> Vec<Parameter> {
        vec![Parameter {
            instance_id: instance_id.to_owned(),
            parameter_id: "quality".to_owned(),
            value: Value::from(self.quality),
            value_type: "integer",
            min: MIN_QUALITY,
            max: MAX_QUALITY,
            is_online_writable: true,
        }]
    }

    fn set_parameter(&mut self, parameter_id: &str, value: Value) -> Result<()> {
        ensure!(
            parameter_id == "quality",
            "unknown online parameter {parameter_id:?}"
        );
        let quality =
            serde_json::from_value(value).context("JPEG quality must be an unsigned integer")?;
        validate_quality(quality)?;
        self.quality = quality;
        Ok(())
    }
}
