//! Wayland virtual-keyboard execution with the active Niri XKB layout.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    os::fd::AsFd,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use nix::sys::memfd::{MFdFlags, memfd_create};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{
    Action, ActionRoute, ErrorCode, InputState, ProtocolError, TextRoutePreference,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use xkbcommon::xkb;

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

const XKB_EVDEV_OFFSET: u32 = 8;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;

/// Executes key, chord, and supported text actions through virtual keyboard v1.
pub struct VirtualKeyboardExecutor {
    layout: String,
    state: Mutex<Option<KeyboardState>>,
}

impl VirtualKeyboardExecutor {
    /// Initialises the device and uploads its keymap before advertising support.
    pub fn initialise(active_layout_name: &str) -> Result<Self, ProtocolError> {
        let layout = xkb_layout_for_niri_name(active_layout_name)?.to_owned();
        Ok(Self {
            state: Mutex::new(Some(KeyboardState::connect(&layout)?)),
            layout,
        })
    }

    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut KeyboardState) -> Result<T, ActionExecutionFailure>,
    ) -> Result<T, ActionExecutionFailure> {
        let mut guard = self.state.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(backend_failure(
                "virtual-keyboard state lock is poisoned",
                false,
            ))
        })?;
        if guard.is_none() {
            *guard = Some(
                KeyboardState::connect(&self.layout)
                    .map_err(ActionExecutionFailure::not_executed)?,
            );
        }
        let result = operation(guard.as_mut().expect("keyboard state was initialised"));
        if matches!(result, Err(ActionExecutionFailure::Indeterminate { .. })) {
            guard.take();
        }
        result
    }
}

impl ActionExecutor for VirtualKeyboardExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        self.with_state(|state| state.execute(action))?;
        Ok(ActionRoute::VirtualKeyboard.into())
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::VirtualKeyboard
    }

    fn release_all(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.take();
        }
    }
}

struct KeyboardState {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    keyboard: ZwpVirtualKeyboardV1,
    strokes: BTreeMap<u32, KeyStroke>,
    shift_keycode: u32,
    modifiers_by_keycode: BTreeMap<u32, u32>,
    depressed_modifiers: u32,
    held_keys: BTreeSet<u32>,
}

impl KeyboardState {
    fn connect(layout: &str) -> Result<Self, ProtocolError> {
        let CompiledKeymap {
            bytes: keymap_bytes,
            strokes,
            shift_keycode,
            modifiers_by_keycode,
        } = compile_keymap(layout)?;

        let connection = Connection::connect_to_env()
            .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
        let (globals, queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|error| backend_failure(format!("read Wayland globals: {error}"), true))?;
        let qh = queue.handle();
        let manager: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|error| {
                backend_failure(format!("bind virtual-keyboard manager: {error}"), false)
            })?;
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|error| backend_failure(format!("bind Wayland seat: {error}"), false))?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

        let keymap_size = u32::try_from(keymap_bytes.len())
            .map_err(|_| backend_failure("compiled keymap exceeds protocol size", false))?;
        let keymap_fd =
            memfd_create("waldo-virtual-keyboard", MFdFlags::MFD_CLOEXEC).map_err(|error| {
                backend_failure(format!("create keymap memory file: {error}"), true)
            })?;
        let mut keymap_file = File::from(keymap_fd);
        keymap_file
            .write_all(&keymap_bytes)
            .map_err(|error| backend_failure(format!("write virtual keymap: {error}"), true))?;
        keyboard.keymap(KEYMAP_FORMAT_XKB_V1, keymap_file.as_fd(), keymap_size);
        connection
            .flush()
            .map_err(|error| backend_failure(format!("create virtual keyboard: {error}"), true))?;

        Ok(Self {
            connection,
            queue,
            keyboard,
            strokes,
            shift_keycode,
            modifiers_by_keycode,
            depressed_modifiers: 0,
            held_keys: BTreeSet::new(),
        })
    }

    fn execute(&mut self, action: &Action) -> Result<(), ActionExecutionFailure> {
        let prepared = self
            .prepare(action)
            .map_err(ActionExecutionFailure::not_executed)?;
        match prepared {
            PreparedKeyboardAction::Key { keycode, state } => self.key(keycode, state),
            PreparedKeyboardAction::Chord(keycodes) => {
                for keycode in &keycodes {
                    self.key(*keycode, InputState::Pressed);
                }
                for keycode in keycodes.iter().rev() {
                    self.key(*keycode, InputState::Released);
                }
            }
            PreparedKeyboardAction::Text(strokes) => {
                for stroke in strokes {
                    if stroke.shift {
                        self.key(self.shift_keycode, InputState::Pressed);
                    }
                    self.key(stroke.keycode, InputState::Pressed);
                    self.key(stroke.keycode, InputState::Released);
                    if stroke.shift {
                        self.key(self.shift_keycode, InputState::Released);
                    }
                }
            }
        }
        self.connection.flush().map_err(|error| {
            ActionExecutionFailure::indeterminate(
                ActionRoute::VirtualKeyboard,
                backend_failure(format!("flush virtual-keyboard action: {error}"), true),
            )
        })?;
        self.queue
            .dispatch_pending(&mut WaylandState)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::VirtualKeyboard,
                    backend_failure(
                        format!("dispatch virtual-keyboard protocol state: {error}"),
                        true,
                    ),
                )
            })?;
        Ok(())
    }

    fn prepare(&self, action: &Action) -> Result<PreparedKeyboardAction, ProtocolError> {
        match action {
            Action::Key { key, state } => Ok(PreparedKeyboardAction::Key {
                keycode: self.resolve_key(key)?,
                state: *state,
            }),
            Action::KeyChord { keys } => {
                if keys.is_empty() {
                    return Err(invalid("key chord must contain at least one key"));
                }
                Ok(PreparedKeyboardAction::Chord(
                    keys.iter()
                        .map(|key| self.resolve_key(key))
                        .collect::<Result<Vec<_>, _>>()?,
                ))
            }
            Action::Text { text, route } => {
                if !matches!(route, TextRoutePreference::VirtualKeyboard) {
                    return Err(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: format!("requested text route {route:?} is not initialised"),
                        retryable: false,
                    });
                }
                Ok(PreparedKeyboardAction::Text(self.resolve_text(text)?))
            }
            _ => Err(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "this action has no virtual-keyboard route".into(),
                retryable: false,
            }),
        }
    }

    fn resolve_key(&self, name: &str) -> Result<u32, ProtocolError> {
        let keysym = named_keysym(name).ok_or_else(|| {
            invalid(format!(
                "key name {name:?} is not recognised by the virtual keyboard"
            ))
        })?;
        self.find_stroke(keysym)
            .map(|stroke| stroke.keycode)
            .ok_or_else(|| invalid(format!("key {name:?} is absent from the virtual keymap")))
    }

    fn resolve_character(&self, character: char) -> Result<KeyStroke, ProtocolError> {
        let keysym = text_keysym(character);
        self.find_stroke(keysym).ok_or_else(|| ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: format!(
                "character {character:?} cannot be typed by the active virtual-keyboard keymap"
            ),
            retryable: false,
        })
    }

    fn resolve_text(&self, text: &str) -> Result<Vec<KeyStroke>, ProtocolError> {
        let mut characters = text.chars().peekable();
        let mut strokes = Vec::with_capacity(text.chars().count());
        while let Some(mut character) = characters.next() {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
                character = '\n';
            }
            strokes.push(self.resolve_character(character)?);
        }
        Ok(strokes)
    }

    fn find_stroke(&self, target: xkb::Keysym) -> Option<KeyStroke> {
        self.strokes.get(&target.raw()).copied()
    }

    fn key(&mut self, keycode: u32, state: InputState) {
        let wire_state = match state {
            InputState::Pressed => {
                self.held_keys.insert(keycode);
                KEY_PRESSED
            }
            InputState::Released => {
                self.held_keys.remove(&keycode);
                KEY_RELEASED
            }
        };
        self.keyboard.key(timestamp(), keycode, wire_state);
        if self.modifiers_by_keycode.contains_key(&keycode) {
            self.depressed_modifiers = self
                .held_keys
                .iter()
                .filter_map(|held| self.modifiers_by_keycode.get(held))
                .fold(0, |state, mask| state | mask);
            self.keyboard.modifiers(self.depressed_modifiers, 0, 0, 0);
        }
    }
}

struct CompiledKeymap {
    bytes: Vec<u8>,
    strokes: BTreeMap<u32, KeyStroke>,
    shift_keycode: u32,
    modifiers_by_keycode: BTreeMap<u32, u32>,
}

fn compile_keymap(layout: &str) -> Result<CompiledKeymap, ProtocolError> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", layout, "", None, xkb::COMPILE_NO_FLAGS)
            .ok_or_else(|| {
                backend_failure(format!("compile active XKB layout {layout:?}"), false)
            })?;

    let mut strokes = BTreeMap::new();
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let xkb_keycode = xkb::Keycode::new(raw);
        let Some(keycode) = evdev_keycode(xkb_keycode) else {
            continue;
        };
        // Supported layouts use level zero for an unmodified key and level one
        // for Shift. Higher levels would require modifiers this
        // backend does not currently synthesise and are deliberately omitted.
        let supported_levels = keymap.num_levels_for_key(xkb_keycode, 0).min(2);
        for level in 0..supported_levels {
            for keysym in keymap.key_get_syms_by_level(xkb_keycode, 0, level) {
                strokes.entry(keysym.raw()).or_insert(KeyStroke {
                    keycode,
                    shift: level == 1,
                });
            }
        }
    }
    let shift_keycode = strokes
        .get(&xkb::keysyms::KEY_Shift_L)
        .map(|stroke| stroke.keycode)
        .ok_or_else(|| backend_failure("compiled keymap has no left Shift key", false))?;
    let mut modifiers_by_keycode = BTreeMap::new();
    for (modifier_name, keysyms) in [
        (
            "Shift",
            [xkb::keysyms::KEY_Shift_L, xkb::keysyms::KEY_Shift_R],
        ),
        (
            "Control",
            [xkb::keysyms::KEY_Control_L, xkb::keysyms::KEY_Control_R],
        ),
        ("Mod1", [xkb::keysyms::KEY_Alt_L, xkb::keysyms::KEY_Alt_R]),
        (
            "Mod4",
            [xkb::keysyms::KEY_Super_L, xkb::keysyms::KEY_Super_R],
        ),
    ] {
        let index = keymap.mod_get_index(modifier_name);
        if index == xkb::MOD_INVALID || index >= u32::BITS {
            return Err(backend_failure(
                format!("compiled keymap has no representable {modifier_name} modifier"),
                false,
            ));
        }
        let mask = 1 << index;
        for keysym in keysyms {
            if let Some(stroke) = strokes.get(&keysym) {
                modifiers_by_keycode.insert(stroke.keycode, mask);
            }
        }
    }
    let mut bytes = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    bytes.push(0);

    Ok(CompiledKeymap {
        bytes,
        strokes,
        shift_keycode,
        modifiers_by_keycode,
    })
}

fn xkb_layout_for_niri_name(name: &str) -> Result<&'static str, ProtocolError> {
    match name {
        "English (UK)" => Ok("gb"),
        "English (US)" => Ok("us"),
        _ => Err(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: format!("active Niri keyboard layout {name:?} has no verified XKB mapping"),
            retryable: false,
        }),
    }
}

impl Drop for KeyboardState {
    fn drop(&mut self) {
        for keycode in std::mem::take(&mut self.held_keys) {
            self.keyboard.key(timestamp(), keycode, KEY_RELEASED);
        }
        if self.depressed_modifiers != 0 {
            self.keyboard.modifiers(0, 0, 0, 0);
        }
        self.keyboard.destroy();
        let _ = self.connection.flush();
    }
}

enum PreparedKeyboardAction {
    Key { keycode: u32, state: InputState },
    Chord(Vec<u32>),
    Text(Vec<KeyStroke>),
}

#[derive(Clone, Copy)]
struct KeyStroke {
    keycode: u32,
    shift: bool,
}

fn named_keysym(name: &str) -> Option<xkb::Keysym> {
    let canonical = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "control_l" => "Control_L",
        "shift" | "shift_l" => "Shift_L",
        "alt" | "alt_l" => "Alt_L",
        "super" | "meta" | "super_l" => "Super_L",
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "backspace" => "BackSpace",
        "tab" => "Tab",
        "space" => "space",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Page_Up",
        "pagedown" | "page_down" => "Page_Down",
        "delete" | "del" => "Delete",
        other if other.chars().count() == 1 => {
            return other.chars().next().map(xkb::Keysym::from_char);
        }
        _ => name,
    };
    let keysym = xkb::keysym_from_name(canonical, xkb::KEYSYM_CASE_INSENSITIVE);
    (keysym != xkb::Keysym::NoSymbol).then_some(keysym)
}

fn text_keysym(character: char) -> xkb::Keysym {
    match character {
        '\n' | '\r' => xkb::keysyms::KEY_Return.into(),
        '\t' => xkb::keysyms::KEY_Tab.into(),
        _ => xkb::Keysym::from_char(character),
    }
}

fn evdev_keycode(keycode: xkb::Keycode) -> Option<u32> {
    keycode.raw().checked_sub(XKB_EVDEV_OFFSET)
}

fn timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled before virtual-keyboard dispatch".into(),
        retryable: false,
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: message.into(),
        retryable: false,
    }
}

fn backend_failure(message: impl Into<String>, retryable: bool) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message: message.into(),
        retryable,
    }
}

#[derive(Default)]
struct WaylandState;

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
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
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
    use super::{
        compile_keymap, evdev_keycode, named_keysym, text_keysym, xkb_layout_for_niri_name,
    };
    use waldo_protocol::ErrorCode;
    use xkbcommon::xkb;

    #[test]
    fn common_key_names_resolve_without_layout_guessing() {
        assert_eq!(named_keysym("Enter"), Some(xkb::keysyms::KEY_Return.into()));
        assert_eq!(
            named_keysym("ctrl"),
            Some(xkb::keysyms::KEY_Control_L.into())
        );
        assert_eq!(named_keysym("A"), Some(xkb::Keysym::from_char('a')));
        assert_eq!(named_keysym("not-a-real-key"), None);
    }

    #[test]
    fn text_control_characters_use_application_keys() {
        assert_eq!(text_keysym('\n'), xkb::keysyms::KEY_Return.into());
        assert_eq!(text_keysym('\r'), xkb::keysyms::KEY_Return.into());
        assert_eq!(text_keysym('\t'), xkb::keysyms::KEY_Tab.into());
        assert_eq!(text_keysym('A'), xkb::Keysym::from_char('A'));
    }

    #[test]
    fn xkb_keycodes_convert_to_wayland_evdev_codes() {
        assert_eq!(evdev_keycode(xkb::Keycode::new(38)), Some(30));
        assert_eq!(evdev_keycode(xkb::Keycode::new(7)), None);
    }

    #[test]
    fn active_layout_mapping_marks_level_one_characters_as_shifted() {
        let keymap = compile_keymap("gb").expect("compile keymap");

        assert!(!keymap.strokes[&xkb::Keysym::from_char('w').raw()].shift);
        assert!(keymap.strokes[&xkb::Keysym::from_char('W').raw()].shift);
        assert!(!keymap.strokes[&xkb::Keysym::from_char('1').raw()].shift);
        assert!(keymap.strokes[&xkb::Keysym::from_char('!').raw()].shift);
        assert!(
            keymap
                .modifiers_by_keycode
                .contains_key(&keymap.shift_keycode)
        );
    }

    #[test]
    fn niri_layout_names_fail_closed_when_the_xkb_mapping_is_unknown() {
        assert_eq!(xkb_layout_for_niri_name("English (UK)").unwrap(), "gb");
        assert_eq!(xkb_layout_for_niri_name("English (US)").unwrap(), "us");
        assert_eq!(
            xkb_layout_for_niri_name("Unverified layout")
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedCapability
        );
    }
}
