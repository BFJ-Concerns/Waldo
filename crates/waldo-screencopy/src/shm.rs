use std::{ffi::CString, fs::File, os::fd::AsFd};

use memmap2::{MmapMut, MmapOptions};
use nix::{
    sys::memfd::{MFdFlags, memfd_create},
    unistd::ftruncate,
};
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm, wl_shm_pool::WlShmPool},
};

use crate::CaptureError;

/// A buffer shape checked against everything later code relies on: both
/// dimensions positive, each row holding a whole line of 4-byte pixels, and
/// every size within the 32-bit limits of the wl_shm requests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BufferLayout {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    /// Bytes the buffer spans: `stride` times `height`.
    pub size: usize,
}

impl BufferLayout {
    pub(crate) fn new(width: u32, height: u32, stride: u32) -> Result<Self, CaptureError> {
        let invalid = |reason: &str| CaptureError::InvalidBuffer(reason.into());
        if width == 0 || height == 0 {
            return Err(invalid("buffer dimensions must be positive"));
        }
        let wire = |value: u32, what: &str| {
            i32::try_from(value).map_err(|_| invalid(&format!("{what} exceeds i32")))
        };
        wire(width, "width")?;
        wire(height, "height")?;
        wire(stride, "stride")?;
        let row_bytes = u64::from(width) * 4;
        if u64::from(stride) < row_bytes {
            return Err(invalid("stride is shorter than one row of pixels"));
        }
        let size = u64::from(stride) * u64::from(height);
        if i32::try_from(size).is_err() {
            return Err(invalid("wl_shm pool exceeds i32"));
        }
        let size = usize::try_from(size).map_err(|_| invalid("buffer size exceeds usize"))?;
        Ok(Self {
            width: width as usize,
            height: height as usize,
            stride: stride as usize,
            size,
        })
    }
}

pub(crate) struct ShmBuffer {
    pub map: MmapMut,
    pub buffer: WlBuffer,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: wl_shm::Format,
}

pub(crate) fn allocate<State>(
    shm: &wl_shm::WlShm,
    queue: &QueueHandle<State>,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
) -> Result<ShmBuffer, CaptureError>
where
    State:
        wayland_client::Dispatch<WlShmPool, ()> + wayland_client::Dispatch<WlBuffer, ()> + 'static,
{
    let size = BufferLayout::new(width, height, stride)?.size;
    let name = CString::new("waldo-screencopy").expect("static memfd name contains no NUL");
    let descriptor = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC)
        .map_err(|error| CaptureError::Io(error.into()))?;
    ftruncate(&descriptor, size as i64).map_err(|error| CaptureError::Io(error.into()))?;
    let file = File::from(descriptor);
    // SAFETY: the memfd has just been resized to `size`, remains owned by
    // `file`, and no other mapping can mutate it outside Wayland's copy.
    let map = unsafe { MmapOptions::new().len(size).map_mut(&file)? };
    // BufferLayout has checked that every value fits i32.
    let pool = shm.create_pool(file.as_fd(), size as i32, queue, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        queue,
        (),
    );
    pool.destroy();

    Ok(ShmBuffer {
        map,
        buffer,
        width,
        height,
        stride,
        format,
    })
}
