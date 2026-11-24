mod capabilities;
mod capture;
mod controls;
mod ffi;
mod media;
mod sensor;
#[allow(dead_code, non_camel_case_types, clippy::all)]
#[path = "uapi/bindings.rs"]
mod uapi;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use capabilities::Capabilities;
pub use capture::{CaptureEvent, CaptureSpec, CapturedFrame, FormatRequest};
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

pub(crate) fn is_device_lost(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| ffi::has_errno(cause, &[libc::ENODEV, libc::ENXIO, libc::EBADF]))
}

pub struct Device {
    file: File,
    caps: ffi::RawCaps,
    capture: capture::CaptureQueue,
    sensor: Option<sensor::SensorControls>,
}

impl Device {
    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = open_node(path).with_context(|| format!("open {}", path.display()))?;
        let caps = ffi::query_caps(file.as_raw_fd())
            .with_context(|| format!("VIDIOC_QUERYCAP on {}", path.display()))?;
        Ok(Self {
            file,
            caps,
            capture: capture::CaptureQueue::default(),
            sensor: None,
        })
    }

    pub(crate) fn discover_sensor_controls(&mut self, video_path: &Path) {
        let sensor = media::open_sensor(&self.file).and_then(|sensor| {
            sensor
                .map(|(path, file)| {
                    let controls = sensor::SensorControls::start(file)?;
                    eprintln!(
                        "cam2web: {} sensor controls via {}",
                        video_path.display(),
                        path.display()
                    );
                    Ok(controls)
                })
                .transpose()
        });
        match sensor {
            Ok(controls) => self.sensor = controls,
            Err(error) => eprintln!(
                "cam2web: {} sensor discovery: {error}",
                video_path.display()
            ),
        }
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

fn open_node(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}
