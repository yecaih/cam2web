mod capabilities;
mod capture;
mod controls;
mod ffi;
#[allow(dead_code, non_camel_case_types, clippy::all)]
#[path = "uapi/bindings.rs"]
mod uapi;

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use capabilities::Capabilities;
pub use capture::{CaptureEvent, CaptureSpec, Captured, FormatRequest};
pub use controls::ControlGroup;
pub(crate) use controls::{ControlScan, ControlValueScan, ReadableControl};

#[allow(unused_imports)]
pub use capabilities::{Format, FrameInterval, FrameSize};
#[allow(unused_imports)]
pub use controls::{ControlKind, MenuItem};

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Fraction {
    pub numerator: u32,
    pub denominator: u32,
}

impl Fraction {
    fn is_valid(&self) -> bool {
        self.numerator != 0 && self.denominator != 0
    }
}

pub struct Device {
    file: File,
    caps: ffi::RawCaps,
    capture: capture::CaptureQueue,
}

impl Device {
    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let caps = ffi::query_caps(file.as_raw_fd())
            .with_context(|| format!("VIDIOC_QUERYCAP on {}", path.display()))?;
        Ok(Self {
            file,
            caps,
            capture: capture::CaptureQueue::default(),
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
