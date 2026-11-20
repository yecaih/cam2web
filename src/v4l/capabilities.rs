use std::io;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use super::ffi::{self, RawInterval, RawSize, has_errno, text};
use super::uapi::{
    V4L2_BUF_TYPE_META_CAPTURE, V4L2_BUF_TYPE_VIDEO_CAPTURE, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
};
use super::{Device, Fraction};

#[derive(Clone, Serialize, Debug)]
pub struct Capabilities {
    /// Eligible by QUERYCAP; MMAP support is confirmed when start requests buffers.
    pub can_stream: bool,
    pub driver: String,
    pub card: String,
    pub bus: String,
    pub capabilities: u32,
    pub device_caps: u32,
    pub effective_caps: u32,
    pub has_streaming: bool,
    pub has_single_plane: bool,
    pub has_mplane: bool,
    pub has_meta: bool,
    pub formats: Vec<Format>,
    pub mplane_formats: Vec<Format>,
    pub meta_formats: Vec<Format>,
}

#[derive(Clone, Serialize, Debug)]
pub struct Format {
    pub fourcc: String,
    pub description: String,
    pub flags: u32,
    pub sizes: Vec<FrameSize>,
}

#[derive(Clone, Serialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameSize {
    Discrete {
        width: u32,
        height: u32,
        intervals: Vec<FrameInterval>,
    },
    Continuous {
        min_width: u32,
        max_width: u32,
        min_height: u32,
        max_height: u32,
    },
    Stepwise {
        min_width: u32,
        max_width: u32,
        step_width: u32,
        min_height: u32,
        max_height: u32,
        step_height: u32,
    },
}

#[derive(Clone, Serialize, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrameInterval {
    Discrete {
        #[serde(flatten)]
        interval: Fraction,
    },
    Continuous {
        min: Fraction,
        max: Fraction,
    },
    Stepwise {
        min: Fraction,
        max: Fraction,
        step: Fraction,
    },
}

#[repr(u32)]
#[derive(Clone, Copy, Debug)]
enum Family {
    SinglePlane = V4L2_BUF_TYPE_VIDEO_CAPTURE,
    Mplane = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
    Meta = V4L2_BUF_TYPE_META_CAPTURE,
}

impl Device {
    pub fn capabilities(&self) -> Result<Capabilities> {
        let caps = &self.caps;
        let has_single_plane = caps.has_single_plane;
        let has_mplane = caps.has_mplane;
        let has_meta = caps.has_meta;
        let has_streaming = caps.has_streaming;
        Ok(Capabilities {
            can_stream: has_single_plane && has_streaming,
            driver: text(&caps.driver),
            card: text(&caps.card),
            bus: text(&caps.bus),
            capabilities: caps.capabilities,
            device_caps: caps.device_caps,
            effective_caps: caps.effective_caps,
            has_streaming,
            has_single_plane,
            has_mplane,
            has_meta,
            formats: self.list_formats(has_single_plane, Family::SinglePlane)?,
            mplane_formats: self.list_formats(has_mplane, Family::Mplane)?,
            meta_formats: self.list_formats(has_meta, Family::Meta)?,
        })
    }

    fn list_formats(&self, is_supported: bool, family: Family) -> Result<Vec<Format>> {
        if !is_supported {
            return Ok(Vec::new());
        }
        enumerate(|index| ffi::enum_format(self.fd(), family as u32, index))
            .with_context(|| format!("VIDIOC_ENUM_FMT for {family:?}"))?
            .into_iter()
            .map(|raw| {
                Ok(Format {
                    fourcc: fourcc_text(raw.pixelformat),
                    description: text(&raw.description),
                    flags: raw.flags,
                    sizes: if matches!(family, Family::Meta) {
                        Vec::new()
                    } else {
                        self.list_sizes(raw.pixelformat)?
                    },
                })
            })
            .collect::<Result<Vec<_>>>()
            .context("enumerate V4L2 formats and sizes")
    }

    fn list_sizes(&self, fourcc: u32) -> Result<Vec<FrameSize>> {
        enumerate(|index| ffi::enum_size(self.fd(), fourcc, index))
            .with_context(|| format!("VIDIOC_ENUM_FRAMESIZES for {}", fourcc_text(fourcc)))?
            .into_iter()
            .map(|raw| {
                Ok(match raw {
                    RawSize::Discrete(width, height) => FrameSize::Discrete {
                        width,
                        height,
                        intervals: self.list_intervals(fourcc, width, height)?,
                    },
                    RawSize::Continuous(range) => FrameSize::Continuous {
                        min_width: range.min_width,
                        max_width: range.max_width,
                        min_height: range.min_height,
                        max_height: range.max_height,
                    },
                    RawSize::Stepwise(range) => FrameSize::Stepwise {
                        min_width: range.min_width,
                        max_width: range.max_width,
                        step_width: range.step_width,
                        min_height: range.min_height,
                        max_height: range.max_height,
                        step_height: range.step_height,
                    },
                    RawSize::Unknown(other) => bail!("unknown V4L2 frame-size type {other}"),
                })
            })
            .collect()
    }

    /// Query a concrete size from a continuous or stepwise size range.
    /// An empty result means the driver did not advertise intervals for this tuple.
    pub fn frame_intervals(
        &self,
        fourcc: &str,
        width: u32,
        height: u32,
    ) -> Result<Vec<FrameInterval>> {
        ensure!(
            width != 0 && height != 0,
            "width and height must be positive"
        );
        self.list_intervals(parse_fourcc(fourcc)?, width, height)
    }

    fn list_intervals(&self, fourcc: u32, width: u32, height: u32) -> Result<Vec<FrameInterval>> {
        enumerate(|index| ffi::enum_interval(self.fd(), fourcc, width, height, index))
            .with_context(|| {
                format!(
                    "VIDIOC_ENUM_FRAMEINTERVALS for {} {width}x{height}",
                    fourcc_text(fourcc)
                )
            })?
            .into_iter()
            .map(|raw| {
                Ok(match raw {
                    RawInterval::Discrete(interval) => FrameInterval::Discrete { interval },
                    RawInterval::Continuous(min, max) => FrameInterval::Continuous { min, max },
                    RawInterval::Stepwise(min, max, step) => {
                        FrameInterval::Stepwise { min, max, step }
                    }
                    RawInterval::Unknown(other) => {
                        bail!("unknown V4L2 frame-interval type {other}")
                    }
                })
            })
            .collect()
    }
}

fn enumerate<T>(mut query: impl FnMut(u32) -> io::Result<T>) -> io::Result<Vec<T>> {
    let mut entries = Vec::new();
    for index in 0..=u32::MAX {
        match query(index) {
            Ok(entry) => entries.push(entry),
            Err(err)
                if has_errno(&err, &[libc::EINVAL])
                    || (index == 0 && has_errno(&err, &[libc::ENOTTY])) =>
            {
                return Ok(entries);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("V4L2 enumeration did not terminate"))
}

pub(super) fn fourcc_text(code: u32) -> String {
    let bytes = (code & 0x7fff_ffff).to_le_bytes();
    if !bytes.iter().all(|byte| (b' '..=b'~').contains(byte)) {
        return format!("0x{code:08x}");
    }
    let mut result = text(&bytes);
    if code & 0x8000_0000 != 0 {
        result.push_str("-BE");
    }
    result
}

pub(super) fn parse_fourcc(value: &str) -> Result<u32> {
    if let Some(hex) = value.strip_prefix("0x").filter(|hex| hex.len() == 8) {
        ensure!(
            hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid hexadecimal FourCC {value:?}"
        );
        return u32::from_str_radix(hex, 16).context("parse hexadecimal FourCC");
    }
    let (code, endian_bit) = value
        .strip_suffix("-BE")
        .map_or((value, 0), |code| (code, 0x8000_0000));
    let bytes: [u8; 4] = code
        .as_bytes()
        .try_into()
        .context("FourCC must contain four ASCII bytes")?;
    ensure!(
        bytes.iter().all(|byte| (b' '..=b'~').contains(byte)),
        "FourCC must use printable ASCII"
    );
    Ok(u32::from_le_bytes(bytes) | endian_bit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fourcc_preserves_trailing_spaces_endianness_and_raw_codes() {
        assert_eq!(parse_fourcc("Y16 ").unwrap(), u32::from_le_bytes(*b"Y16 "));
        for fourcc in ["Y16 ", "ARGB-BE", "0x00000001"] {
            assert_eq!(fourcc_text(parse_fourcc(fourcc).unwrap()), fourcc);
        }
        assert!(parse_fourcc("Y16").is_err());
        assert!(parse_fourcc("\u{00e9}12").is_err());
    }

    #[test]
    fn discrete_format_serializes_with_flat_fractions() {
        let size = FrameSize::Discrete {
            width: 640,
            height: 480,
            intervals: vec![FrameInterval::Discrete {
                interval: Fraction {
                    numerator: 1,
                    denominator: 30,
                },
            }],
        };
        assert_eq!(
            serde_json::to_value(size).unwrap(),
            json!({
                "kind": "discrete", "width": 640, "height": 480,
                "intervals": [{"kind": "discrete", "numerator": 1, "denominator": 30}]
            })
        );
    }
}
