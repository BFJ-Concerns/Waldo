//! The Wayland side: the seat's keyboard, whose keymap event reports the
//! compositor's actual keymap, and the virtual keyboard that keymap is
//! uploaded to.

use std::{
    fs::File,
    io::Write,
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::FileExt,
    },
};

use nix::sys::memfd::{MFdFlags, memfd_create};
use waldo_protocol::ProtocolError;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_keyboard, wl_registry, wl_seat},
};

mod protocol {
    pub mod client {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("./protocol/virtual-keyboard-unstable-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("./protocol/virtual-keyboard-unstable-v1.xml");
    }
}

use protocol::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const KEYMAP_FORMAT_XKB_V1: u32 = 1;
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// The connection, the seat keyboard listened to for keymap events, and the
/// virtual keyboard driven.
pub(crate) struct Device {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    state: WaylandState,
    keyboard: ZwpVirtualKeyboardV1,
    _seat_keyboard: Option<wl_keyboard::WlKeyboard>,
}

#[derive(Default)]
pub(crate) struct WaylandState {
    has_keyboard: bool,
    /// The most recent keymap the seat reported, not yet taken.
    pending_keymap: Option<Result<String, ProtocolError>>,
}

impl Device {
    /// Connects, listens to the seat's keyboard, and creates the virtual
    /// keyboard. Returns the compositor's keymap text when the seat
    /// reported one.
    pub(crate) fn connect() -> Result<(Self, Option<String>), ProtocolError> {
        let connection = Connection::connect_to_env().map_err(|error| {
            ProtocolError::backend_failure(format!("connect to Wayland: {error}"), true)
        })?;
        let (globals, mut queue) =
            registry_queue_init::<WaylandState>(&connection).map_err(|error| {
                ProtocolError::backend_failure(format!("read Wayland globals: {error}"), true)
            })?;
        let qh = queue.handle();
        let manager: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|error| {
                ProtocolError::backend_failure(
                    format!("bind virtual-keyboard manager: {error}"),
                    false,
                )
            })?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).map_err(|error| {
            ProtocolError::backend_failure(format!("bind Wayland seat: {error}"), false)
        })?;

        let mut state = WaylandState::default();
        queue.roundtrip(&mut state).map_err(|error| {
            ProtocolError::backend_failure(format!("read seat capabilities: {error}"), true)
        })?;
        let seat_keyboard = state.has_keyboard.then(|| seat.get_keyboard(&qh, ()));
        if seat_keyboard.is_some() {
            queue.roundtrip(&mut state).map_err(|error| {
                ProtocolError::backend_failure(format!("read seat keymap: {error}"), true)
            })?;
        }
        let compositor_keymap = state.pending_keymap.take().transpose()?;

        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        Ok((
            Self {
                connection,
                queue,
                state,
                keyboard,
                _seat_keyboard: seat_keyboard,
            },
            compositor_keymap,
        ))
    }

    /// Uploads keymap text to the virtual keyboard.
    pub(crate) fn upload_keymap(&mut self, text: &str) -> Result<(), ProtocolError> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let size = u32::try_from(bytes.len())
            .map_err(|_| ProtocolError::backend_failure("keymap exceeds protocol size", false))?;
        let fd =
            memfd_create("waldo-virtual-keyboard", MFdFlags::MFD_CLOEXEC).map_err(|error| {
                ProtocolError::backend_failure(format!("create keymap memory file: {error}"), true)
            })?;
        let mut file = File::from(fd);
        file.write_all(&bytes).map_err(|error| {
            ProtocolError::backend_failure(format!("write virtual keymap: {error}"), true)
        })?;
        self.keyboard
            .keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), size);
        self.connection.flush().map_err(|error| {
            ProtocolError::backend_failure(format!("upload virtual keymap: {error}"), true)
        })
    }

    /// Reads any events the compositor has sent since the last call and
    /// returns a keymap the seat reported in the meantime.
    pub(crate) fn poll_keymap(&mut self) -> Result<Option<String>, ProtocolError> {
        self.connection.flush().map_err(|error| {
            ProtocolError::backend_failure(format!("flush Wayland connection: {error}"), true)
        })?;
        if let Some(guard) = self.queue.prepare_read() {
            // Nothing to read is the ordinary case; any other failure means
            // the connection is gone and the keyboard state is stale.
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(ProtocolError::backend_failure(
                        format!("read Wayland events: {error}"),
                        true,
                    ));
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| {
                ProtocolError::backend_failure(format!("dispatch Wayland events: {error}"), true)
            })?;
        self.state.pending_keymap.take().transpose()
    }

    pub(crate) fn key(&self, keycode: u32, pressed: bool) {
        self.keyboard.key(
            waldo_action::input_event_time_ms(),
            keycode,
            if pressed { KEY_PRESSED } else { KEY_RELEASED },
        );
    }

    pub(crate) fn modifiers(&self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.keyboard.modifiers(depressed, latched, locked, group);
    }

    #[cfg(test)]
    pub(crate) fn disconnect_for_test(&self) {
        use std::os::fd::AsRawFd;
        nix::sys::socket::shutdown(
            self.connection.as_fd().as_raw_fd(),
            nix::sys::socket::Shutdown::Both,
        )
        .unwrap();
    }

    /// Flushes and dispatches after an action; failure means the action's
    /// delivery is unknown.
    pub(crate) fn settle(&mut self) -> Result<(), ProtocolError> {
        self.connection.flush().map_err(|error| {
            ProtocolError::backend_failure(format!("flush virtual-keyboard action: {error}"), true)
        })?;
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| {
                ProtocolError::backend_failure(
                    format!("dispatch virtual-keyboard protocol state: {error}"),
                    true,
                )
            })?;
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.keyboard.destroy();
        let _ = self.connection.flush();
    }
}

/// Largest keymap accepted from the compositor. Real XKB keymaps are tens
/// of kilobytes; the bound keeps a malformed size from exhausting memory.
const MAX_KEYMAP_BYTES: u32 = 4 * 1024 * 1024;

/// Reads the keymap the compositor shared. The descriptor may be a
/// duplicate whose file offset is not at the start, so the read names its
/// offset rather than trusting the shared position.
fn read_keymap(fd: OwnedFd, size: u32) -> Result<String, ProtocolError> {
    if size == 0 || size > MAX_KEYMAP_BYTES {
        return Err(ProtocolError::backend_failure(
            format!("the compositor keymap size {size} is outside 1..={MAX_KEYMAP_BYTES} bytes"),
            false,
        ));
    }
    let file = File::from(fd);
    let mut bytes = vec![0; size as usize];
    file.read_exact_at(&mut bytes, 0).map_err(|error| {
        ProtocolError::backend_failure(format!("read the compositor keymap: {error}"), true)
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ProtocolError::backend_failure("the compositor keymap is not UTF-8", false))?;
    Ok(text.trim_end_matches('\0').to_owned())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.pending_keymap = Some(match format {
                WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) => read_keymap(fd, size),
                other => Err(ProtocolError::unsupported_capability(format!(
                    "the compositor keymap format {other:?} is not XKB v1"
                ))),
            });
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: protocol::client::zwp_virtual_keyboard_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: protocol::client::zwp_virtual_keyboard_v1::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_KEYMAP_BYTES, read_keymap};

    #[test]
    fn keymap_sizes_outside_the_bound_are_refused_before_reading() {
        for size in [0, MAX_KEYMAP_BYTES + 1, u32::MAX] {
            let file = std::fs::File::open("/dev/null").unwrap();
            let error = read_keymap(file.into(), size).unwrap_err();
            assert!(
                error.message.contains("outside"),
                "{size}: {}",
                error.message
            );
        }
    }
}
