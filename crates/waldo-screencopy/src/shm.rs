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
    let size = usize::try_from(stride)
        .ok()
        .and_then(|stride| {
            usize::try_from(height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .ok_or_else(|| CaptureError::InvalidBuffer("buffer size overflow".into()))?;
    let name = CString::new("waldo-screencopy").expect("static memfd name contains no NUL");
    let descriptor = memfd_create(name.as_c_str(), MFdFlags::MFD_CLOEXEC)
        .map_err(|error| CaptureError::Io(error.into()))?;
    ftruncate(
        &descriptor,
        i64::try_from(size).map_err(|_| {
            CaptureError::InvalidBuffer("buffer is too large for the platform".into())
        })?,
    )
    .map_err(|error| CaptureError::Io(error.into()))?;
    let file = File::from(descriptor);
    // SAFETY: the memfd has just been resized to `size`, remains owned by
    // `file`, and no other mapping can mutate it outside Wayland's copy.
    let map = unsafe { MmapOptions::new().len(size).map_mut(&file)? };
    let pool = shm.create_pool(
        file.as_fd(),
        i32::try_from(size)
            .map_err(|_| CaptureError::InvalidBuffer("wl_shm pool exceeds i32".into()))?,
        queue,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        i32::try_from(width)
            .map_err(|_| CaptureError::InvalidBuffer("width exceeds i32".into()))?,
        i32::try_from(height)
            .map_err(|_| CaptureError::InvalidBuffer("height exceeds i32".into()))?,
        i32::try_from(stride)
            .map_err(|_| CaptureError::InvalidBuffer("stride exceeds i32".into()))?,
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
