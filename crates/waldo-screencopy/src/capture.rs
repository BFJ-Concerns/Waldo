use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::poll::{PollFd, PollFlags, poll};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool},
};

use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::{
    CaptureError, encode,
    shm::{self, ShmBuffer},
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// One completed in-memory capture, oriented to the output's logical desktop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured_at_unix_ms: u64,
}

/// Stateless entry point for a single Niri output capture.
#[derive(Clone, Copy, Debug, Default)]
pub struct Screencopy;

impl Screencopy {
    pub fn capture_output(&self, output_name: &str) -> Result<Capture, CaptureError> {
        self.capture(output_name, None, true)
    }

    pub fn capture_output_without_cursor(
        &self,
        output_name: &str,
    ) -> Result<Capture, CaptureError> {
        self.capture(output_name, None, false)
    }

    pub fn capture_region(
        &self,
        output_name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Capture, CaptureError> {
        self.capture(output_name, Some((x, y, width, height)), true)
    }

    pub fn capture_region_without_cursor(
        &self,
        output_name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Capture, CaptureError> {
        self.capture(output_name, Some((x, y, width, height)), false)
    }

    fn capture(
        &self,
        output_name: &str,
        region: Option<(i32, i32, i32, i32)>,
        overlay_cursor: bool,
    ) -> Result<Capture, CaptureError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| CaptureError::Wayland(error.to_string()))?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| CaptureError::Wayland(error.to_string()))?;
        let handle = queue.handle();
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&handle, 1..=1, ())
            .map_err(|_| CaptureError::ProtocolMissing("wl_shm"))?;
        let manager = globals
            .bind::<ZwlrScreencopyManagerV1, _, _>(&handle, 1..=3, ())
            .map_err(|_| CaptureError::ProtocolMissing("zwlr_screencopy_manager_v1"))?;
        let mut state = State::new(shm);

        let mut too_old = None;
        globals.contents().with_list(|list| {
            for global in list.iter().filter(|global| global.interface == "wl_output") {
                // Version 4 is the first that reports the output names
                // captures are requested by.
                if global.version < 4 {
                    too_old = Some(global.version);
                    continue;
                }
                let output = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    4,
                    &handle,
                    (),
                );
                state.outputs.push(OutputRecord {
                    output,
                    name: None,
                    transform: None,
                });
            }
        });
        if let Some(version) = too_old {
            return Err(CaptureError::Wayland(format!(
                "the compositor offers wl_output version {version}; capture needs version 4 for output names"
            )));
        }
        queue
            .roundtrip(&mut state)
            .map_err(|error| CaptureError::Wayland(error.to_string()))?;
        let output = state
            .outputs
            .iter()
            .find(|output| output.name.as_deref() == Some(output_name))
            .ok_or_else(|| CaptureError::OutputMissing(output_name.to_owned()))?;
        state.transform = output.transform.ok_or_else(|| {
            CaptureError::Wayland("output did not report a supported transform".into())
        })?;
        let output = output.output.clone();

        let frame = match region {
            Some((x, y, width, height)) => manager.capture_output_region(
                overlay_cursor as i32,
                &output,
                x,
                y,
                width,
                height,
                &handle,
                (),
            ),
            None => manager.capture_output(overlay_cursor as i32, &output, &handle, ()),
        };
        state.frame = Some(frame);
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        while state.result.is_none() {
            dispatch_until(&mut queue, &mut state, deadline)?;
        }
        state
            .result
            .take()
            .expect("capture loop exits only with a result")
    }
}

/// Dispatch available Wayland events without allowing a silent compositor to
/// retain one of the service's blocking workers forever.
fn dispatch_until(
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    deadline: Instant,
) -> Result<(), CaptureError> {
    queue
        .flush()
        .map_err(|error| CaptureError::Wayland(error.to_string()))?;
    if queue
        .dispatch_pending(state)
        .map_err(|error| CaptureError::Wayland(error.to_string()))?
        > 0
    {
        return Ok(());
    }

    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(CaptureError::TimedOut);
    };
    let Some(read_guard) = queue.prepare_read() else {
        return Ok(());
    };
    let mut descriptors = [PollFd::new(read_guard.connection_fd(), PollFlags::POLLIN)];
    let timeout_ms = remaining.as_millis().clamp(1, u128::from(u16::MAX)) as u16;
    let ready =
        poll(&mut descriptors, timeout_ms).map_err(|error| CaptureError::Io(error.into()))?;
    if ready == 0 {
        return Err(CaptureError::TimedOut);
    }
    read_guard
        .read()
        .map_err(|error| CaptureError::Wayland(error.to_string()))?;
    queue
        .dispatch_pending(state)
        .map_err(|error| CaptureError::Wayland(error.to_string()))?;
    Ok(())
}

struct OutputRecord {
    output: wl_output::WlOutput,
    name: Option<String>,
    transform: Option<wl_output::Transform>,
}

struct BufferOffer {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct State {
    shm: wl_shm::WlShm,
    outputs: Vec<OutputRecord>,
    frame: Option<ZwlrScreencopyFrameV1>,
    offer: Option<BufferOffer>,
    buffer: Option<ShmBuffer>,
    y_inverted: bool,
    transform: wl_output::Transform,
    result: Option<Result<Capture, CaptureError>>,
}

impl State {
    fn new(shm: wl_shm::WlShm) -> Self {
        Self {
            shm,
            outputs: Vec::new(),
            frame: None,
            offer: None,
            buffer: None,
            y_inverted: false,
            transform: wl_output::Transform::Normal,
            result: None,
        }
    }

    fn begin_copy(&mut self, queue: &QueueHandle<Self>) {
        let Some(offer) = self.offer.take() else {
            self.result = Some(Err(CaptureError::UnsupportedFormat(
                "compositor offered no wl_shm format".into(),
            )));
            return;
        };
        match shm::allocate(
            &self.shm,
            queue,
            offer.width,
            offer.height,
            offer.stride,
            offer.format,
        ) {
            Ok(buffer) => {
                if let Some(frame) = &self.frame {
                    frame.copy(&buffer.buffer);
                }
                self.buffer = Some(buffer);
            }
            Err(error) => self.result = Some(Err(error)),
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        if let Some(record) = state.outputs.iter_mut().find(|item| item.output == *proxy) {
            match event {
                wl_output::Event::Name { name } => record.name = Some(name),
                wl_output::Event::Geometry { transform, .. } => {
                    record.transform = match transform {
                        WEnum::Value(transform) => Some(transform),
                        WEnum::Unknown(_) => None,
                    };
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _connection: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => match format {
                WEnum::Value(format @ (wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)) => {
                    state.offer = Some(BufferOffer {
                        format,
                        width,
                        height,
                        stride,
                    });
                    if proxy.version() < 3 {
                        state.begin_copy(queue);
                    }
                }
                WEnum::Value(other) => {
                    state.result = Some(Err(CaptureError::UnsupportedFormat(format!("{other:?}"))));
                }
                WEnum::Unknown(value) => {
                    state.result = Some(Err(CaptureError::UnsupportedFormat(value.to_string())));
                }
            },
            zwlr_screencopy_frame_v1::Event::BufferDone => state.begin_copy(queue),
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.y_inverted = match flags {
                    WEnum::Value(flags) => flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
                    WEnum::Unknown(_) => false,
                };
            }
            zwlr_screencopy_frame_v1::Event::Damage { .. } => {
                // Damage is change metadata, not a crop. The compositor copies into
                // the complete offered buffer and Ready always encodes that full extent.
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                // Timestamp the completed frame before PNG encoding. Encoding can be
                // comparatively slow and must not make fresh evidence look newer than it is.
                let captured_at_unix_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
                let result = match &state.buffer {
                    Some(buffer) => captured_at_unix_ms
                        .map_err(|error| CaptureError::Wayland(error.to_string()))
                        .and_then(|captured_at_unix_ms| {
                            encode::png(buffer, state.y_inverted, state.transform).map(
                                |(png, width, height)| Capture {
                                    png,
                                    width,
                                    height,
                                    captured_at_unix_ms,
                                },
                            )
                        }),
                    None => Err(CaptureError::InvalidBuffer(
                        "ready arrived before a copy buffer was created".into(),
                    )),
                };
                proxy.destroy();
                state.result = Some(result);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                proxy.destroy();
                state.result = Some(Err(CaptureError::CompositorFailed));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timeout_never_exceeds_the_remaining_capture_budget() {
        let remaining = Duration::from_millis(37);
        let timeout_ms = remaining.as_millis().clamp(1, u128::from(u16::MAX)) as u16;

        assert_eq!(timeout_ms, 37);
    }

    #[test]
    fn sub_millisecond_capture_budget_still_gets_one_poll_tick() {
        let remaining = Duration::from_nanos(1);
        let timeout_ms = remaining.as_millis().clamp(1, u128::from(u16::MAX)) as u16;

        assert_eq!(timeout_ms, 1);
    }
}
