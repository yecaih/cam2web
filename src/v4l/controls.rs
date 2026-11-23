use std::{io, os::fd::RawFd};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::ffi::{self, RawMenu, has_errno, text};
use super::{Device, is_device_lost};

#[derive(Clone, Copy, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Integer,
    Boolean,
    Menu,
    Button,
    Integer64,
    ControlClass,
    String,
    Bitmask,
    IntegerMenu,
    U8,
    U16,
    U32,
    Compound,
    Unknown,
}

impl ControlKind {
    fn from_raw(kind: u32) -> Self {
        match kind {
            1 => Self::Integer,
            2 => Self::Boolean,
            3 => Self::Menu,
            4 => Self::Button,
            5 => Self::Integer64,
            6 => Self::ControlClass,
            7 => Self::String,
            8 => Self::Bitmask,
            9 => Self::IntegerMenu,
            0x0100 => Self::U8,
            0x0101 => Self::U16,
            0x0102 => Self::U32,
            0x0103.. => Self::Compound,
            _ => Self::Unknown,
        }
    }

    fn is_scalar(self) -> bool {
        matches!(
            self,
            Self::Integer
                | Self::Boolean
                | Self::Menu
                | Self::Button
                | Self::Integer64
                | Self::Bitmask
                | Self::IntegerMenu
        )
    }
}

#[derive(Clone, Serialize, Debug)]
pub struct Control {
    pub id: u32,
    pub name: String,
    pub kind: ControlKind,
    pub type_id: u32,
    pub min: i64,
    pub max: i64,
    pub step: u64,
    pub default: i64,
    pub value: Option<i64>,
    pub flags: u32,
    pub menu: Vec<MenuItem>,
    pub can_read: bool,
    pub can_write: bool,
    pub is_volatile: bool,
    pub has_payload: bool,
    pub elem_size: u32,
    pub elems: u32,
    pub dims: Vec<u32>,
}

#[derive(Clone, Serialize, Debug)]
pub struct MenuItem {
    pub index: u32,
    pub name: Option<String>,
    pub value: Option<i64>,
}

const CTRL_DISABLED: u32 = 0x0001;
const CTRL_GRABBED: u32 = 0x0002;
const CTRL_READ_ONLY: u32 = 0x0004;
const CTRL_INACTIVE: u32 = 0x0010;
const CTRL_WRITE_ONLY: u32 = 0x0040;
const CTRL_VOLATILE: u32 = 0x0080;
const CTRL_HAS_PAYLOAD: u32 = 0x0100;
const CTRL_MODIFY_LAYOUT: u32 = 0x0400;
const CTRL_ID_MASK: u32 = 0x0fff_ffff;
const CTRL_CLASS_MASK: u32 = 0x0fff_0000;
const CTRL_CLASS_USER: u32 = 0x0098_0000;
const CTRL_CLASS_CAMERA: u32 = 0x009a_0000;

#[derive(Clone, Copy)]
pub enum ControlGroup {
    User,
    Camera,
}

impl ControlGroup {
    pub(crate) fn includes(self, id: u32) -> bool {
        let class = match self {
            Self::User => CTRL_CLASS_USER,
            Self::Camera => CTRL_CLASS_CAMERA,
        };
        id & CTRL_CLASS_MASK == class
    }
}

impl ffi::RawControl {
    fn kind(&self) -> ControlKind {
        ControlKind::from_raw(self.type_id)
    }

    fn has_payload(&self) -> bool {
        self.flags & CTRL_HAS_PAYLOAD != 0
    }

    fn is_scalar(&self) -> bool {
        !self.has_payload() && self.nr_of_dims == 0 && self.elems <= 1 && self.kind().is_scalar()
    }

    fn can_read(&self) -> bool {
        self.is_scalar()
            && !matches!(self.kind(), ControlKind::Button)
            && self.flags & (CTRL_DISABLED | CTRL_WRITE_ONLY) == 0
    }

    fn can_write(&self) -> bool {
        self.is_scalar()
            && self.flags & (CTRL_DISABLED | CTRL_INACTIVE | CTRL_READ_ONLY | CTRL_GRABBED) == 0
    }
}

fn enumerate_controls(fd: RawFd, group: ControlGroup) -> Result<Vec<ffi::RawControl>> {
    let mut descriptors = Vec::new();
    let mut cursor = 0;
    loop {
        let raw = match ffi::query_control(fd, cursor, true) {
            Ok(raw) => raw,
            Err(error) if has_errno(&error, &[libc::EINVAL]) => return Ok(descriptors),
            Err(error) if cursor == 0 && has_errno(&error, &[libc::ENOTTY]) => {
                return Ok(descriptors);
            }
            Err(error) => return Err(error).context("enumerate V4L2 controls"),
        };
        ensure!(
            raw.id > cursor,
            "VIDIOC_QUERY_EXT_CTRL returned a non-increasing control ID"
        );
        cursor = raw.id;
        if raw.flags & CTRL_DISABLED == 0
            && !matches!(raw.kind(), ControlKind::ControlClass)
            && group.includes(raw.id)
        {
            descriptors.push(raw);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ReadableControl {
    pub(crate) id: u32,
    type_id: u32,
}

impl Control {
    pub(crate) fn readable(&self) -> Option<ReadableControl> {
        self.can_read.then_some(ReadableControl {
            id: self.id,
            type_id: self.type_id,
        })
    }
}

#[derive(Serialize)]
pub(crate) struct ControlReading {
    id: u32,
    value: Option<i64>,
}

pub(crate) struct ControlValueScan {
    remaining: std::vec::IntoIter<ReadableControl>,
    readings: Vec<ControlReading>,
}

impl ControlValueScan {
    pub(crate) fn new(controls: Vec<ReadableControl>) -> Self {
        Self {
            readings: Vec::with_capacity(controls.len()),
            remaining: controls.into_iter(),
        }
    }

    pub(crate) fn step(&mut self, device: &Device) -> Result<Option<Vec<ControlReading>>> {
        if let Some(control) = self.remaining.next() {
            self.readings.push(ControlReading {
                id: control.id,
                value: device.read_scalar(control)?,
            });
        }
        Ok(self
            .remaining
            .as_slice()
            .is_empty()
            .then(|| std::mem::take(&mut self.readings)))
    }
}

pub(crate) struct ControlScan {
    descriptors: std::vec::IntoIter<ffi::RawControl>,
    controls: Vec<Control>,
}

impl ControlScan {
    pub(crate) fn step(&mut self, device: &Device) -> Result<Option<Vec<Control>>> {
        if let Some(raw) = self.descriptors.next() {
            self.controls.push(device.read_control(&raw)?);
        }
        Ok(self
            .descriptors
            .as_slice()
            .is_empty()
            .then(|| std::mem::take(&mut self.controls)))
    }
}

impl Device {
    pub(crate) fn scan_controls(&self, group: ControlGroup) -> Result<ControlScan> {
        let descriptors = enumerate_controls(self.fd(), group)?;
        Ok(ControlScan {
            controls: Vec::with_capacity(descriptors.len()),
            descriptors: descriptors.into_iter(),
        })
    }

    fn read_scalar(&self, control: ReadableControl) -> Result<Option<i64>> {
        match ffi::scalar_control(self.fd(), control.id, control.type_id, None) {
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                let error = anyhow::Error::new(error);
                if is_device_lost(&error) {
                    return Err(error)
                        .with_context(|| format!("VIDIOC_G_EXT_CTRLS for {:#x}", control.id));
                }
                Ok(None)
            }
        }
    }

    fn read_control(&self, raw: &ffi::RawControl) -> Result<Control> {
        let can_read = raw.can_read();
        let value = if can_read {
            self.read_scalar(ReadableControl {
                id: raw.id,
                type_id: raw.type_id,
            })?
        } else {
            None
        };
        Ok(Control {
            id: raw.id,
            name: text(&raw.name),
            kind: raw.kind(),
            type_id: raw.type_id,
            min: raw.min,
            max: raw.max,
            step: raw.step,
            default: raw.default_value,
            value,
            flags: raw.flags,
            menu: self.list_menu(raw)?,
            can_read,
            can_write: self.can_write_control(raw),
            is_volatile: raw.flags & CTRL_VOLATILE != 0,
            has_payload: raw.has_payload(),
            elem_size: raw.elem_size,
            elems: raw.elems,
            dims: raw.dims[..(raw.nr_of_dims as usize).min(raw.dims.len())].to_vec(),
        })
    }

    fn menu_item(&self, raw: &ffi::RawControl, index: u32) -> io::Result<MenuItem> {
        let is_integer = matches!(raw.kind(), ControlKind::IntegerMenu);
        let (name, value) = match ffi::query_menu(self.fd(), raw.id, index, is_integer)? {
            RawMenu::Name(name) => (Some(text(&name)), None),
            RawMenu::Value(value) => (None, Some(value)),
        };
        Ok(MenuItem { index, name, value })
    }

    fn list_menu(&self, raw: &ffi::RawControl) -> Result<Vec<MenuItem>> {
        if !matches!(raw.kind(), ControlKind::Menu | ControlKind::IntegerMenu)
            || raw.flags & CTRL_DISABLED != 0
        {
            return Ok(Vec::new());
        }
        let mut items = Vec::new();
        for index in u32::try_from(raw.min)?..=u32::try_from(raw.max)? {
            match self.menu_item(raw, index) {
                Ok(item) => items.push(item),
                Err(err) if has_errno(&err, &[libc::EINVAL]) => (),
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("VIDIOC_QUERYMENU {:#x}[{index}]", raw.id));
                }
            }
        }
        Ok(items)
    }

    fn scalar_control(&self, raw: &ffi::RawControl, value: Option<i64>) -> Result<i64> {
        ffi::scalar_control(self.fd(), raw.id, raw.type_id, value).with_context(|| {
            let operation = if value.is_some() {
                "VIDIOC_S_EXT_CTRLS"
            } else {
                "VIDIOC_G_EXT_CTRLS"
            };
            format!("{operation} for {:#x}", raw.id)
        })
    }

    fn can_write_control(&self, raw: &ffi::RawControl) -> bool {
        raw.can_write() && !(self.capture.has_buffers() && raw.flags & CTRL_MODIFY_LAYOUT != 0)
    }

    pub fn set_control(&self, group: ControlGroup, id: u32, value: i64) -> Result<Option<i64>> {
        ensure!(id & !CTRL_ID_MASK == 0, "invalid V4L2 control ID {id:#x}");
        ensure!(
            group.includes(id),
            "control {id:#x} is not in the requested V4L2 control class"
        );
        let raw = ffi::query_control(self.fd(), id, false)
            .with_context(|| format!("VIDIOC_QUERY_EXT_CTRL {id:#x}"))?;
        ensure!(
            raw.id == id,
            "driver returned the wrong control ID for {id:#x}"
        );
        ensure!(
            self.can_write_control(&raw),
            "control {id:#x} is not a writable scalar in the current state (flags {:#x})",
            raw.flags
        );
        let value = match raw.kind() {
            ControlKind::Button => 0,
            ControlKind::Bitmask => {
                ensure!(
                    u32::try_from(value).is_ok() && value & !raw.max == 0,
                    "invalid bitmask for control {id:#x}"
                );
                value
            }
            kind => {
                ensure!(
                    (raw.min..=raw.max).contains(&value),
                    "value out of range for control {id:#x} ({}..={})",
                    raw.min,
                    raw.max
                );
                if !matches!(kind, ControlKind::Integer64) {
                    i32::try_from(value).context("V4L2 scalar must fit i32")?;
                }
                if matches!(kind, ControlKind::Menu | ControlKind::IntegerMenu) {
                    self.menu_item(&raw, u32::try_from(value)?)
                        .with_context(|| {
                            format!("unavailable menu index {value} for control {id:#x}")
                        })?;
                }
                value
            }
        };
        self.scalar_control(&raw, Some(value))?;
        if !raw.can_read() {
            return Ok(None);
        }
        let refreshed = ffi::query_control(self.fd(), id, false).with_context(|| {
            format!("control {id:#x} written; failed to refresh its descriptor")
        })?;
        ensure!(
            refreshed.id == id,
            "control {id:#x} written; driver returned the wrong control ID for readback"
        );
        if !refreshed.can_read() {
            return Ok(None);
        }
        self.scalar_control(&refreshed, None)
            .map(Some)
            .with_context(|| format!("control {id:#x} written; failed to read back its value"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_match_only_their_v4l2_class() {
        assert!(ControlGroup::Camera.includes(0x009a_2064));
        assert!(ControlGroup::Camera.includes(0x009a_0901));
        assert!(!ControlGroup::Camera.includes(0x0098_1b00));
        assert!(!ControlGroup::Camera.includes(0x009e_0001));
        assert!(!ControlGroup::User.includes(0x009a_0901));
        assert!(ControlGroup::User.includes(0x0098_1b00));
        assert!(ControlGroup::User.includes(0x0098_0900));
        assert!(!ControlGroup::User.includes(0x009e_0001));
    }

    #[test]
    fn control_access_excludes_payload_and_respects_flags() {
        let mut raw = ffi::RawControl {
            type_id: 1,
            elems: 1,
            ..Default::default()
        };
        assert!(raw.can_read() && raw.can_write());
        raw.flags = CTRL_GRABBED;
        assert!(raw.can_read() && !raw.can_write());
        raw.flags = CTRL_INACTIVE;
        assert!(raw.can_read() && !raw.can_write());
        raw.flags = CTRL_WRITE_ONLY;
        assert!(!raw.can_read() && raw.can_write());
        raw.flags = CTRL_HAS_PAYLOAD;
        assert!(!raw.can_read() && !raw.can_write());
        raw.flags = 0;
        raw.type_id = 0x0102;
        assert!(!raw.can_read() && !raw.can_write());
    }
}
