use std::{io, marker::PhantomData, os::fd::RawFd};

use linux_raw_sys::{
    general::{_IOC_SIZEMASK, _IOC_SIZESHIFT},
    ioctl as numbers,
};

use super::{Fraction, uapi::*};

struct Request<Payload> {
    number: u32,
    payload: PhantomData<fn(&mut Payload)>,
}

impl<Payload> Request<Payload> {
    const fn new(number: u32) -> Self {
        assert!(((number >> _IOC_SIZESHIFT) & _IOC_SIZEMASK) as usize == size_of::<Payload>());
        Self {
            number,
            payload: PhantomData,
        }
    }

    fn call(&self, fd: RawFd, payload: &mut Payload) -> io::Result<()> {
        loop {
            // Each request is paired with its UAPI type; scalar extended controls
            // also keep the pointed-to control alive until the synchronous ioctl returns.
            let result = unsafe { libc::ioctl(fd, self.number as _, std::ptr::from_mut(payload)) };
            if result >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
    }
}

const QUERY_CAP: Request<v4l2_capability> = Request::new(numbers::VIDIOC_QUERYCAP);
const ENUM_FORMAT: Request<v4l2_fmtdesc> = Request::new(numbers::VIDIOC_ENUM_FMT);
const ENUM_SIZE: Request<v4l2_frmsizeenum> = Request::new(numbers::VIDIOC_ENUM_FRAMESIZES);
const ENUM_INTERVAL: Request<v4l2_frmivalenum> = Request::new(numbers::VIDIOC_ENUM_FRAMEINTERVALS);
const QUERY_CONTROL: Request<v4l2_query_ext_ctrl> = Request::new(numbers::VIDIOC_QUERY_EXT_CTRL);
const QUERY_MENU: Request<v4l2_querymenu> = Request::new(numbers::VIDIOC_QUERYMENU);
const GET_CONTROL: Request<v4l2_ext_controls> = Request::new(numbers::VIDIOC_G_EXT_CTRLS);
const SET_CONTROL: Request<v4l2_ext_controls> = Request::new(numbers::VIDIOC_S_EXT_CTRLS);
const SET_FORMAT: Request<v4l2_format> = Request::new(numbers::VIDIOC_S_FMT);
const GET_INTERVAL: Request<v4l2_streamparm> = Request::new(numbers::VIDIOC_G_PARM);
const SET_INTERVAL: Request<v4l2_streamparm> = Request::new(numbers::VIDIOC_S_PARM);
const REQUEST_BUFFERS: Request<v4l2_requestbuffers> = Request::new(numbers::VIDIOC_REQBUFS);
const QUERY_BUFFER: Request<v4l2_buffer> = Request::new(numbers::VIDIOC_QUERYBUF);
const QUEUE_BUFFER: Request<v4l2_buffer> = Request::new(numbers::VIDIOC_QBUF);
const DEQUEUE_BUFFER: Request<v4l2_buffer> = Request::new(numbers::VIDIOC_DQBUF);
const STREAM_ON: Request<libc::c_int> = Request::new(numbers::VIDIOC_STREAMON);
const STREAM_OFF: Request<libc::c_int> = Request::new(numbers::VIDIOC_STREAMOFF);

#[derive(Debug)]
pub(super) struct RawCaps {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus: [u8; 32],
    pub capabilities: u32,
    pub device_caps: u32,
    pub effective_caps: u32,
    pub has_single_plane: bool,
    pub has_mplane: bool,
    pub has_meta: bool,
    pub has_streaming: bool,
}

pub(super) fn query_caps(fd: RawFd) -> io::Result<RawCaps> {
    let mut caps = v4l2_capability::default();
    QUERY_CAP.call(fd, &mut caps)?;
    let effective_caps = if caps.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        caps.device_caps
    } else {
        caps.capabilities
    };
    Ok(RawCaps {
        driver: caps.driver,
        card: caps.card,
        bus: caps.bus_info,
        capabilities: caps.capabilities,
        device_caps: caps.device_caps,
        effective_caps,
        has_single_plane: effective_caps & V4L2_CAP_VIDEO_CAPTURE != 0,
        has_mplane: effective_caps & V4L2_CAP_VIDEO_CAPTURE_MPLANE != 0,
        has_meta: effective_caps & V4L2_CAP_META_CAPTURE != 0,
        has_streaming: effective_caps & V4L2_CAP_STREAMING != 0,
    })
}

pub(super) fn enum_format(fd: RawFd, buffer_type: u32, index: u32) -> io::Result<v4l2_fmtdesc> {
    let mut format = v4l2_fmtdesc {
        index,
        type_: buffer_type,
        ..Default::default()
    };
    ENUM_FORMAT.call(fd, &mut format)?;
    Ok(format)
}

pub(super) enum RawSize {
    Discrete(u32, u32),
    Continuous(v4l2_frmsize_stepwise),
    Stepwise(v4l2_frmsize_stepwise),
    Unknown(u32),
}

pub(super) fn enum_size(fd: RawFd, fourcc: u32, index: u32) -> io::Result<RawSize> {
    let mut size = v4l2_frmsizeenum {
        index,
        pixel_format: fourcc,
        ..Default::default()
    };
    ENUM_SIZE.call(fd, &mut size)?;
    // The returned type selects the valid union member.
    Ok(unsafe {
        match size.type_ {
            V4L2_FRMSIZE_TYPE_DISCRETE => {
                let discrete = size.__bindgen_anon_1.discrete;
                RawSize::Discrete(discrete.width, discrete.height)
            }
            V4L2_FRMSIZE_TYPE_CONTINUOUS => RawSize::Continuous(size.__bindgen_anon_1.stepwise),
            V4L2_FRMSIZE_TYPE_STEPWISE => RawSize::Stepwise(size.__bindgen_anon_1.stepwise),
            other => RawSize::Unknown(other),
        }
    })
}

impl From<v4l2_fract> for Fraction {
    fn from(value: v4l2_fract) -> Self {
        Self {
            numerator: value.numerator,
            denominator: value.denominator,
        }
    }
}

pub(super) enum RawInterval {
    Discrete(Fraction),
    Continuous(Fraction, Fraction),
    Stepwise(Fraction, Fraction, Fraction),
    Unknown(u32),
}

pub(super) fn enum_interval(
    fd: RawFd,
    fourcc: u32,
    width: u32,
    height: u32,
    index: u32,
) -> io::Result<RawInterval> {
    let mut interval = v4l2_frmivalenum {
        index,
        pixel_format: fourcc,
        width,
        height,
        ..Default::default()
    };
    ENUM_INTERVAL.call(fd, &mut interval)?;
    // Both kinds of ranges use the same stepwise member.
    Ok(unsafe {
        match interval.type_ {
            V4L2_FRMIVAL_TYPE_DISCRETE => {
                RawInterval::Discrete(interval.__bindgen_anon_1.discrete.into())
            }
            V4L2_FRMIVAL_TYPE_CONTINUOUS => {
                let range = interval.__bindgen_anon_1.stepwise;
                RawInterval::Continuous(range.min.into(), range.max.into())
            }
            V4L2_FRMIVAL_TYPE_STEPWISE => {
                let range = interval.__bindgen_anon_1.stepwise;
                RawInterval::Stepwise(range.min.into(), range.max.into(), range.step.into())
            }
            other => RawInterval::Unknown(other),
        }
    })
}

#[derive(Default)]
pub(super) struct RawControl {
    pub id: u32,
    pub type_id: u32,
    pub name: [u8; 32],
    pub min: i64,
    pub max: i64,
    pub default_value: i64,
    pub step: u64,
    pub flags: u32,
    pub elem_size: u32,
    pub elems: u32,
    pub nr_of_dims: u32,
    pub dims: [u32; 4],
}

pub(super) fn query_control(fd: RawFd, id: u32, is_next: bool) -> io::Result<RawControl> {
    let mut control = v4l2_query_ext_ctrl {
        id: if is_next {
            id | V4L2_CTRL_FLAG_NEXT_CTRL | V4L2_CTRL_FLAG_NEXT_COMPOUND
        } else {
            id
        },
        ..Default::default()
    };
    QUERY_CONTROL.call(fd, &mut control)?;
    let decode = |value: i64| {
        if control.type_ == V4L2_CTRL_TYPE_BITMASK {
            i64::from(value as u32)
        } else {
            value
        }
    };
    Ok(RawControl {
        id: control.id,
        type_id: control.type_,
        name: control.name.map(|byte| byte as u8),
        min: decode(control.minimum),
        max: decode(control.maximum),
        default_value: decode(control.default_value),
        step: control.step,
        flags: control.flags,
        elem_size: control.elem_size,
        elems: control.elems,
        nr_of_dims: control.nr_of_dims,
        dims: control.dims,
    })
}

pub(super) enum RawMenu {
    Name([u8; 32]),
    Value(i64),
}

pub(super) fn query_menu(fd: RawFd, id: u32, index: u32, is_integer: bool) -> io::Result<RawMenu> {
    let mut menu = v4l2_querymenu {
        id,
        index,
        ..Default::default()
    };
    QUERY_MENU.call(fd, &mut menu)?;
    // Read the packed union by value, never by reference.
    Ok(unsafe {
        if is_integer {
            RawMenu::Value(menu.__bindgen_anon_1.value)
        } else {
            RawMenu::Name(menu.__bindgen_anon_1.name)
        }
    })
}

pub(super) fn scalar_control(
    fd: RawFd,
    id: u32,
    type_id: u32,
    requested: Option<i64>,
) -> io::Result<i64> {
    let mut control = v4l2_ext_control {
        id,
        ..Default::default()
    };
    if type_id == V4L2_CTRL_TYPE_INTEGER64 {
        control.__bindgen_anon_1.value64 = requested.unwrap_or_default();
    } else {
        control.__bindgen_anon_1.value = requested.unwrap_or_default() as i32;
    }
    let mut controls = v4l2_ext_controls {
        __bindgen_anon_1: v4l2_ext_controls__bindgen_ty_1 {
            which: V4L2_CTRL_WHICH_CUR_VAL,
        },
        count: 1,
        controls: &raw mut control,
        ..Default::default()
    };
    let request = if requested.is_some() {
        SET_CONTROL
    } else {
        GET_CONTROL
    };
    request.call(fd, &mut controls)?;
    // Match the kernel control type and preserve the unsigned bitmask bits.
    Ok(unsafe {
        match type_id {
            V4L2_CTRL_TYPE_INTEGER64 => control.__bindgen_anon_1.value64,
            V4L2_CTRL_TYPE_BITMASK => i64::from(control.__bindgen_anon_1.value as u32),
            _ => i64::from(control.__bindgen_anon_1.value),
        }
    })
}

pub(super) struct RawSpec {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub size_bytes: u32,
}

pub(super) fn set_format(fd: RawFd, fourcc: u32, width: u32, height: u32) -> io::Result<RawSpec> {
    let mut format = v4l2_format {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        ..Default::default()
    };
    format.fmt.pix = v4l2_pix_format {
        width,
        height,
        pixelformat: fourcc,
        field: V4L2_FIELD_ANY,
        ..Default::default()
    };
    SET_FORMAT.call(fd, &mut format)?;
    // Single-plane capture uses the pix member of the format union.
    let pix = unsafe { format.fmt.pix };
    Ok(RawSpec {
        fourcc: pix.pixelformat,
        width: pix.width,
        height: pix.height,
        stride_bytes: pix.bytesperline,
        size_bytes: pix.sizeimage,
    })
}

pub(super) fn stream_interval(
    fd: RawFd,
    requested: Option<&Fraction>,
) -> io::Result<(Fraction, bool)> {
    let mut params = v4l2_streamparm {
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        ..Default::default()
    };
    let request = if let Some(interval) = requested {
        params.parm.capture = v4l2_captureparm {
            timeperframe: v4l2_fract {
                numerator: interval.numerator,
                denominator: interval.denominator,
            },
            ..Default::default()
        };
        SET_INTERVAL
    } else {
        GET_INTERVAL
    };
    request.call(fd, &mut params)?;
    // The selected capture type determines this union member.
    let capture = unsafe { params.parm.capture };
    Ok((
        capture.timeperframe.into(),
        capture.capability & V4L2_CAP_TIMEPERFRAME != 0,
    ))
}

pub(super) fn request_buffers(fd: RawFd, count: u32) -> io::Result<u32> {
    let mut buffers = v4l2_requestbuffers {
        count,
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        memory: V4L2_MEMORY_MMAP,
        ..Default::default()
    };
    REQUEST_BUFFERS.call(fd, &mut buffers)?;
    Ok(buffers.count)
}

fn mmap_buffer(index: u32) -> v4l2_buffer {
    v4l2_buffer {
        index,
        type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
        memory: V4L2_MEMORY_MMAP,
        ..Default::default()
    }
}

pub(super) fn query_buffer(fd: RawFd, index: u32) -> io::Result<(u32, u32)> {
    let mut buffer = mmap_buffer(index);
    QUERY_BUFFER.call(fd, &mut buffer)?;
    // For a single-plane MMAP buffer, the driver returns the byte offset.
    Ok((unsafe { buffer.m.offset }, buffer.length))
}

pub(super) fn queue_buffer(fd: RawFd, index: u32) -> io::Result<()> {
    QUEUE_BUFFER.call(fd, &mut mmap_buffer(index))
}

pub(super) struct RawFrame {
    pub index: u32,
    pub used_bytes: u32,
    pub sequence: u32,
    pub is_error: bool,
}

pub(super) fn dequeue_buffer(fd: RawFd) -> io::Result<RawFrame> {
    let mut buffer = mmap_buffer(0);
    DEQUEUE_BUFFER.call(fd, &mut buffer)?;
    Ok(RawFrame {
        index: buffer.index,
        used_bytes: buffer.bytesused,
        sequence: buffer.sequence,
        is_error: buffer.flags & V4L2_BUF_FLAG_ERROR != 0,
    })
}

pub(super) fn stream(fd: RawFd, is_on: bool) -> io::Result<()> {
    let request = if is_on { STREAM_ON } else { STREAM_OFF };
    request.call(fd, &mut (V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int))
}

pub(super) fn has_errno(err: &io::Error, codes: &[i32]) -> bool {
    err.raw_os_error().is_some_and(|code| codes.contains(&code))
}

pub(super) fn text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
