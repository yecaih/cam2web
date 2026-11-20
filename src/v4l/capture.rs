use std::io;
use std::os::fd::RawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use super::capabilities::{fourcc_text, parse_fourcc};
use super::ffi::{self, has_errno, is_device_lost};
use super::{Device, Fraction};

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FormatRequest {
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    pub interval: Option<Fraction>,
}

#[derive(Clone, Serialize, Debug)]
pub struct CaptureSpec {
    pub fourcc: String,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub size_bytes: u32,
    pub interval: Option<Fraction>,
}

pub struct Captured {
    pub bytes: Vec<u8>,
    pub sequence: u32,
}

pub enum CaptureEvent {
    Frame(Captured),
    DriverError { sequence: u32 },
    Empty { sequence: u32 },
    Timeout,
}

const BUFFER_COUNT: u32 = 4;

#[derive(Default)]
pub(super) struct CaptureQueue {
    mappings: Vec<Mapping>,
    has_buffers: bool,
    is_streaming: bool,
}

impl CaptureQueue {
    pub(super) fn has_buffers(&self) -> bool {
        self.has_buffers
    }
}

impl Device {
    pub fn start(&mut self, request: &FormatRequest) -> Result<CaptureSpec> {
        ensure!(
            self.caps.has_streaming && self.caps.has_single_plane,
            "this backend requires single-plane V4L2 streaming (mplane and metadata capture are unsupported)"
        );
        ensure!(
            request.width != 0 && request.height != 0,
            "width and height must be positive"
        );
        if let Some(interval) = &request.interval {
            ensure!(
                interval.is_valid(),
                "frame interval numerator and denominator must be positive"
            );
        }
        let fourcc = parse_fourcc(&request.fourcc)?;
        self.stop()
            .context("release previous V4L2 stream before starting")?;
        let raw = ffi::set_format(self.fd(), fourcc, request.width, request.height)
            .context("VIDIOC_S_FMT")?;
        let interval = self.negotiate_interval(request.interval.as_ref())?;
        if let Err(err) = self.start_stream() {
            return match self.stop() {
                Ok(()) => Err(err),
                Err(cleanup) if is_device_lost(&cleanup) && !is_device_lost(&err) => Err(cleanup
                    .context(format!(
                        "stream startup failed: {err:#}; cleanup also failed"
                    ))),
                Err(cleanup) => {
                    Err(err.context(format!("stream startup cleanup failed: {cleanup:#}")))
                }
            };
        }
        Ok(CaptureSpec {
            fourcc: fourcc_text(raw.fourcc),
            width: raw.width,
            height: raw.height,
            stride_bytes: raw.stride_bytes,
            size_bytes: raw.size_bytes,
            interval,
        })
    }

    fn negotiate_interval(&self, requested: Option<&Fraction>) -> Result<Option<Fraction>> {
        let (mut actual, can_set_interval) = match ffi::stream_interval(self.fd(), None) {
            Ok(interval) => interval,
            Err(err) if has_errno(&err, &[libc::EINVAL, libc::ENOTTY]) => return Ok(None),
            Err(err) => return Err(err).context("VIDIOC_G_PARM"),
        };
        if let Some(requested) = requested.filter(|_| can_set_interval) {
            (actual, _) =
                ffi::stream_interval(self.fd(), Some(requested)).context("VIDIOC_S_PARM")?;
        }
        Ok(actual.is_valid().then_some(actual))
    }

    fn start_stream(&mut self) -> Result<()> {
        let count =
            ffi::request_buffers(self.fd(), BUFFER_COUNT).context("VIDIOC_REQBUFS (MMAP)")?;
        self.capture.has_buffers = count != 0;
        ensure!(self.capture.has_buffers, "driver allocated no MMAP buffers");
        for index in 0..count {
            let mapping = Mapping::map(self.fd(), index)?;
            self.capture.mappings.push(mapping);
        }
        for index in 0..count {
            self.queue(index)?;
        }
        ffi::stream(self.fd(), true).context("VIDIOC_STREAMON")?;
        self.capture.is_streaming = true;
        Ok(())
    }

    fn queue(&self, index: u32) -> Result<()> {
        ffi::queue_buffer(self.fd(), index).with_context(|| format!("VIDIOC_QBUF buffer {index}"))
    }

    pub fn stop(&mut self) -> Result<()> {
        if self.capture.is_streaming {
            ffi::stream(self.fd(), false).context("VIDIOC_STREAMOFF")?;
            self.capture.is_streaming = false;
        }
        self.capture.mappings.clear();
        if self.capture.has_buffers {
            ffi::request_buffers(self.fd(), 0).context("VIDIOC_REQBUFS (release)")?;
            self.capture.has_buffers = false;
        }
        Ok(())
    }

    pub fn capture(&mut self, timeout_ms: i32) -> Result<CaptureEvent> {
        ensure!(self.capture.is_streaming, "V4L2 capture is not running");
        let deadline = u64::try_from(timeout_ms)
            .ok()
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            if !wait_readable(self.fd(), deadline)? {
                return Ok(CaptureEvent::Timeout);
            }
            match ffi::dequeue_buffer(self.fd()) {
                Ok(raw) => return self.copy_frame_and_requeue(&raw),
                Err(err) if has_errno(&err, &[libc::EAGAIN]) => (),
                Err(err) => return Err(err).context("VIDIOC_DQBUF"),
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(CaptureEvent::Timeout);
            }
        }
    }

    fn copy_frame_and_requeue(&self, raw: &ffi::RawFrame) -> Result<CaptureEvent> {
        let mapping = self
            .capture
            .mappings
            .get(raw.index as usize)
            .ok_or_else(|| anyhow!("VIDIOC_DQBUF returned invalid buffer index {}", raw.index))?;
        let frame = if raw.is_error {
            Ok(CaptureEvent::DriverError {
                sequence: raw.sequence,
            })
        } else if raw.used_bytes == 0 {
            Ok(CaptureEvent::Empty {
                sequence: raw.sequence,
            })
        } else {
            mapping.copy_bytes(raw.used_bytes).map(|bytes| {
                CaptureEvent::Frame(Captured {
                    bytes,
                    sequence: raw.sequence,
                })
            })
        };
        self.queue(raw.index)?;
        frame
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // If STREAMOFF fails, unmap before File closes and releases the queue.
        let _ = self.stop();
        self.capture.mappings.clear();
    }
}

struct Mapping {
    address: NonNull<u8>,
    len_bytes: usize,
}

impl Mapping {
    fn map(fd: RawFd, index: u32) -> Result<Self> {
        let (offset_bytes, len_bytes) = ffi::query_buffer(fd, index)
            .with_context(|| format!("VIDIOC_QUERYBUF buffer {index}"))?;
        let len_bytes = len_bytes as usize;
        let offset_bytes = map_offset(offset_bytes)?;
        // The driver supplies this buffer's length and file offset via QUERYBUF.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset_bytes,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error()).with_context(|| format!("mmap buffer {index}"));
        }
        if let Some(address) = NonNull::new(address.cast()) {
            return Ok(Self { address, len_bytes });
        }
        unsafe { libc::munmap(address, len_bytes) };
        bail!("mmap returned a null address for buffer {index}");
    }

    fn copy_bytes(&self, used_bytes: u32) -> Result<Vec<u8>> {
        let used_bytes = used_bytes as usize;
        ensure!(
            used_bytes <= self.len_bytes && used_bytes <= isize::MAX as usize,
            "V4L2 bytesused ({used_bytes}) exceeds safe mapped buffer length ({})",
            self.len_bytes
        );
        // DQBUF transferred ownership to userspace until the matching QBUF.
        Ok(unsafe { std::slice::from_raw_parts(self.address.as_ptr(), used_bytes) }.to_vec())
    }
}

fn map_offset(offset_bytes: u32) -> Result<libc::off_t> {
    let offset = offset_bytes as libc::off_t;
    ensure!(offset >= 0, "V4L2 MMAP offset does not fit off_t");
    Ok(offset)
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.address.as_ptr().cast(), self.len_bytes) };
    }
}

fn wait_readable(fd: RawFd, deadline: Option<Instant>) -> Result<bool> {
    loop {
        let timeout_ms = deadline.map_or(-1, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32
        });
        let mut events = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut events, 1, timeout_ms) };
        if result == 0 {
            return Ok(false);
        }
        if result < 0 {
            let err = io::Error::last_os_error();
            if !has_errno(&err, &[libc::EINTR]) {
                return Err(err).context("poll V4L2 device");
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
            continue;
        }
        if events.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF))
                .context("poll V4L2 device: POLLNVAL");
        }
        if events.revents & libc::POLLHUP != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENODEV))
                .context("poll V4L2 device: POLLHUP");
        }
        if events.revents & libc::POLLIN != 0 {
            return Ok(true);
        }
        bail!("V4L2 poll returned unexpected events {:#x}", events.revents);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_request_rejects_unknown_fields_at_both_levels() {
        use serde_json::json;

        let request = json!({"fourcc": "YUYV", "width": 1600, "height": 1200});
        assert!(serde_json::from_value::<FormatRequest>(request.clone()).is_ok());

        let mut bad_request = request.clone();
        bad_request["intervals"] = json!([]);
        assert!(serde_json::from_value::<FormatRequest>(bad_request).is_err());

        let mut bad_fraction = request;
        bad_fraction["interval"] = json!({"numerator": 1, "denominator": 30, "denomenator": 30});
        assert!(serde_json::from_value::<FormatRequest>(bad_fraction).is_err());
    }

    #[test]
    fn poll_and_ioctl_handle_a_non_camera_fd() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        let fd = reader.as_raw_fd();
        assert!(!wait_readable(fd, Some(Instant::now())).unwrap());
        writer.write_all(&[1]).unwrap();
        assert!(wait_readable(fd, Some(Instant::now())).unwrap());
        let err = ffi::query_caps(fd).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOTTY));
        drop(writer);
        let err = wait_readable(fd, Some(Instant::now())).unwrap_err();
        assert!(is_device_lost(&err));
    }
}
