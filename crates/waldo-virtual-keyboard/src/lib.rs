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
    named_keycodes: BTreeMap<u32, u32>,
    shift_keycode: u32,
    shift_mask: u32,
    model: KeyboardModel,
}

impl KeyboardState {
    fn connect(layout: &str) -> Result<Self, ProtocolError> {
        let CompiledKeymap {
            bytes: keymap_bytes,
            strokes,
            named_keycodes,
            shift_keycode,
            shift_mask,
            modifiers_by_keycode,
            locking_modifiers_by_keycode,
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
            named_keycodes,
            shift_keycode,
            shift_mask,
            model: KeyboardModel::new(modifiers_by_keycode, locking_modifiers_by_keycode),
        })
    }

    fn execute(&mut self, action: &Action) -> Result<(), ActionExecutionFailure> {
        let prepared = self
            .prepare(action)
            .map_err(ActionExecutionFailure::not_executed)?;
        let transitions = self
            .model
            .plan(prepared, self.shift_keycode, self.shift_mask)
            .map_err(ActionExecutionFailure::not_executed)?;
        for transition in transitions {
            self.emit(transition);
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
                if self.model.has_active_modifiers() {
                    return Err(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "virtual-keyboard text is unavailable while an explicit modifier or lock state is active"
                            .into(),
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
        self.named_keycodes
            .get(&keysym.raw())
            .copied()
            .ok_or_else(|| invalid(format!("key {name:?} is absent from the virtual keymap")))
    }

    fn resolve_character(&self, character: char) -> Result<KeyStroke, ProtocolError> {
        let keysym = text_keysym(character);
        self.find_stroke(keysym)
            .filter(|stroke| stroke.modifiers == 0 || stroke.modifiers == self.shift_mask)
            .ok_or_else(|| ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "character {character:?} cannot be typed exactly by the active virtual-keyboard keymap"
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

    fn emit(&self, transition: KeyboardTransition) {
        match transition {
            KeyboardTransition::Key { keycode, state } => self.keyboard.key(
                timestamp(),
                keycode,
                match state {
                    InputState::Pressed => KEY_PRESSED,
                    InputState::Released => KEY_RELEASED,
                },
            ),
            KeyboardTransition::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => self.keyboard.modifiers(depressed, latched, locked, group),
        }
    }
}

struct CompiledKeymap {
    bytes: Vec<u8>,
    strokes: BTreeMap<u32, KeyStroke>,
    named_keycodes: BTreeMap<u32, u32>,
    shift_keycode: u32,
    shift_mask: u32,
    modifiers_by_keycode: BTreeMap<u32, u32>,
    locking_modifiers_by_keycode: BTreeMap<u32, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyboardTransition {
    Key {
        keycode: u32,
        state: InputState,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

struct KeyboardModel {
    modifiers_by_keycode: BTreeMap<u32, u32>,
    locking_modifiers_by_keycode: BTreeMap<u32, u32>,
    depressed_modifiers: u32,
    locked_modifiers: u32,
    held_keys: BTreeSet<u32>,
}

impl KeyboardModel {
    fn new(
        modifiers_by_keycode: BTreeMap<u32, u32>,
        locking_modifiers_by_keycode: BTreeMap<u32, u32>,
    ) -> Self {
        Self {
            modifiers_by_keycode,
            locking_modifiers_by_keycode,
            depressed_modifiers: 0,
            locked_modifiers: 0,
            held_keys: BTreeSet::new(),
        }
    }

    fn has_active_modifiers(&self) -> bool {
        self.depressed_modifiers != 0 || self.locked_modifiers != 0
    }

    fn clear_modifier_state(&mut self) -> Option<KeyboardTransition> {
        if !self.has_active_modifiers() {
            return None;
        }
        self.depressed_modifiers = 0;
        self.locked_modifiers = 0;
        Some(KeyboardTransition::Modifiers {
            depressed: 0,
            latched: 0,
            locked: 0,
            group: 0,
        })
    }

    fn plan(
        &mut self,
        prepared: PreparedKeyboardAction,
        shift_keycode: u32,
        shift_mask: u32,
    ) -> Result<Vec<KeyboardTransition>, ProtocolError> {
        if let PreparedKeyboardAction::Text(strokes) = &prepared
            && strokes
                .iter()
                .any(|stroke| stroke.modifiers != 0 && stroke.modifiers != shift_mask)
        {
            return Err(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "virtual-keyboard text requires an unsupported non-Shift modifier".into(),
                retryable: false,
            });
        }
        let mut transitions = Vec::new();
        match prepared {
            PreparedKeyboardAction::Key { keycode, state } => {
                self.push_key(&mut transitions, keycode, state);
            }
            PreparedKeyboardAction::Chord(mut keycodes) => {
                keycodes.sort_by_key(|keycode| !self.modifiers_by_keycode.contains_key(keycode));
                for keycode in &keycodes {
                    self.push_key(&mut transitions, *keycode, InputState::Pressed);
                }
                for keycode in keycodes.iter().rev() {
                    self.push_key(&mut transitions, *keycode, InputState::Released);
                }
            }
            PreparedKeyboardAction::Text(strokes) => {
                for stroke in strokes {
                    let shifted = stroke.modifiers != 0;
                    if shifted {
                        self.push_key(&mut transitions, shift_keycode, InputState::Pressed);
                    }
                    self.push_key(&mut transitions, stroke.keycode, InputState::Pressed);
                    self.push_key(&mut transitions, stroke.keycode, InputState::Released);
                    if shifted {
                        self.push_key(&mut transitions, shift_keycode, InputState::Released);
                    }
                }
            }
        }
        Ok(transitions)
    }

    fn push_key(
        &mut self,
        transitions: &mut Vec<KeyboardTransition>,
        keycode: u32,
        state: InputState,
    ) {
        let was_held = self.held_keys.contains(&keycode);
        match state {
            InputState::Pressed => {
                self.held_keys.insert(keycode);
            }
            InputState::Released => {
                self.held_keys.remove(&keycode);
            }
        }
        transitions.push(KeyboardTransition::Key { keycode, state });

        let mut modifiers_changed = self.modifiers_by_keycode.contains_key(&keycode);
        if state == InputState::Pressed
            && !was_held
            && let Some(mask) = self.locking_modifiers_by_keycode.get(&keycode)
        {
            self.locked_modifiers ^= mask;
            modifiers_changed = true;
        }
        if modifiers_changed {
            self.depressed_modifiers = self
                .held_keys
                .iter()
                .filter_map(|held| self.modifiers_by_keycode.get(held))
                .fold(0, |state, mask| state | mask);
            transitions.push(KeyboardTransition::Modifiers {
                depressed: self.depressed_modifiers,
                latched: 0,
                locked: self.locked_modifiers,
                group: 0,
            });
        }
    }
}

fn compile_keymap(layout: &str) -> Result<CompiledKeymap, ProtocolError> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", layout, "", None, xkb::COMPILE_NO_FLAGS)
            .ok_or_else(|| {
                backend_failure(format!("compile active XKB layout {layout:?}"), false)
            })?;

    let shift_mask = modifier_mask(&keymap, "Shift")?;
    let mut strokes = BTreeMap::new();
    let mut named_keycodes = BTreeMap::new();
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let xkb_keycode = xkb::Keycode::new(raw);
        let Some(keycode) = evdev_keycode(xkb_keycode) else {
            continue;
        };
        // The text route deliberately supports only unmodified and Shift
        // levels. Compose, AltGr, dead-key and input-method paths fail closed.
        let supported_levels = keymap.num_levels_for_key(xkb_keycode, 0).min(2);
        for level in 0..supported_levels {
            let mut masks = [0; 8];
            let mask_count = keymap.key_get_mods_for_level(xkb_keycode, 0, level, &mut masks);
            for keysym in keymap.key_get_syms_by_level(xkb_keycode, 0, level) {
                named_keycodes.entry(keysym.raw()).or_insert(keycode);
            }
            let available_masks = &masks[..mask_count.min(masks.len())];
            let Some(modifiers) = available_masks
                .iter()
                .copied()
                .find(|mask| *mask == 0 || *mask == shift_mask)
            else {
                continue;
            };
            for keysym in keymap.key_get_syms_by_level(xkb_keycode, 0, level) {
                strokes
                    .entry(keysym.raw())
                    .or_insert(KeyStroke { keycode, modifiers });
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
        let mask = modifier_mask(&keymap, modifier_name)?;
        for keysym in keysyms {
            if let Some(stroke) = strokes.get(&keysym) {
                modifiers_by_keycode.insert(stroke.keycode, mask);
            }
        }
    }
    let mut locking_modifiers_by_keycode = BTreeMap::new();
    for (modifier_name, keysym) in [
        ("Lock", xkb::keysyms::KEY_Caps_Lock),
        ("Mod2", xkb::keysyms::KEY_Num_Lock),
    ] {
        let mask = modifier_mask(&keymap, modifier_name)?;
        let keycode = named_keycodes.get(&keysym).ok_or_else(|| {
            backend_failure(
                format!("compiled keymap has no {modifier_name} lock key"),
                false,
            )
        })?;
        locking_modifiers_by_keycode.insert(*keycode, mask);
    }
    let mut bytes = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    bytes.push(0);

    Ok(CompiledKeymap {
        bytes,
        strokes,
        named_keycodes,
        shift_keycode,
        shift_mask,
        modifiers_by_keycode,
        locking_modifiers_by_keycode,
    })
}

fn modifier_mask(keymap: &xkb::Keymap, name: &str) -> Result<u32, ProtocolError> {
    let index = keymap.mod_get_index(name);
    if index == xkb::MOD_INVALID || index >= u32::BITS {
        return Err(backend_failure(
            format!("compiled keymap has no representable {name} modifier"),
            false,
        ));
    }
    Ok(1 << index)
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
        let held_keys = self.model.held_keys.iter().copied().collect::<Vec<_>>();
        for keycode in held_keys {
            let transitions = self
                .model
                .plan(
                    PreparedKeyboardAction::Key {
                        keycode,
                        state: InputState::Released,
                    },
                    self.shift_keycode,
                    self.shift_mask,
                )
                .expect("explicit key release never requires text modifiers");
            for transition in transitions {
                self.emit(transition);
            }
        }
        if let Some(transition) = self.model.clear_modifier_state() {
            self.emit(transition);
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
    modifiers: u32,
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
        "capslock" | "caps_lock" => "Caps_Lock",
        "numlock" | "num_lock" => "Num_Lock",
        "keypad1" | "numpad1" | "kp1" => "KP_1",
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
        CompiledKeymap, KeyStroke, KeyboardModel, KeyboardTransition, PreparedKeyboardAction,
        XKB_EVDEV_OFFSET, compile_keymap, evdev_keycode, named_keysym, text_keysym,
        xkb_layout_for_niri_name,
    };
    use waldo_protocol::{ErrorCode, InputState};
    use xkbcommon::xkb;

    #[test]
    fn common_key_names_resolve_without_layout_guessing() {
        assert_eq!(named_keysym("Enter"), Some(xkb::keysyms::KEY_Return.into()));
        assert_eq!(
            named_keysym("ctrl"),
            Some(xkb::keysyms::KEY_Control_L.into())
        );
        assert_eq!(named_keysym("A"), Some(xkb::Keysym::from_char('a')));
        assert_eq!(
            named_keysym("CapsLock"),
            Some(xkb::keysyms::KEY_Caps_Lock.into())
        );
        assert_eq!(named_keysym("Numpad1"), Some(xkb::keysyms::KEY_KP_1.into()));
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

        assert_eq!(
            keymap.strokes[&xkb::Keysym::from_char('w').raw()].modifiers,
            0
        );
        assert_eq!(
            keymap.strokes[&xkb::Keysym::from_char('W').raw()].modifiers,
            keymap.shift_mask
        );
        assert_eq!(
            keymap.strokes[&xkb::Keysym::from_char('1').raw()].modifiers,
            0
        );
        assert_eq!(
            keymap.strokes[&xkb::Keysym::from_char('!').raw()].modifiers,
            keymap.shift_mask
        );
        assert!(
            keymap
                .modifiers_by_keycode
                .contains_key(&keymap.shift_keycode)
        );
    }

    #[test]
    fn chord_modifiers_surround_keys_even_when_the_request_lists_them_last() {
        let keymap = compile_keymap("gb").expect("compile keymap");
        let shift = keycode(&keymap, xkb::keysyms::KEY_Shift_L.into());
        let at = keycode(&keymap, xkb::Keysym::from_char('@'));
        let mut model = model(&keymap);

        let transitions = model
            .plan(
                PreparedKeyboardAction::Chord(vec![at, shift]),
                shift,
                keymap.shift_mask,
            )
            .expect("plan chord");

        assert_eq!(
            key_events(&transitions),
            vec![
                (shift, InputState::Pressed),
                (at, InputState::Pressed),
                (at, InputState::Released),
                (shift, InputState::Released),
            ]
        );
        assert_eq!(
            pressed_symbols("gb", &transitions),
            vec![
                xkb::keysyms::KEY_Shift_L.into(),
                xkb::Keysym::from_char('@')
            ]
        );
    }

    #[test]
    fn locks_persist_and_preserve_keypad_identity_at_the_consumer() {
        let keymap = compile_keymap("gb").expect("compile keymap");
        let shift = keycode(&keymap, xkb::keysyms::KEY_Shift_L.into());
        let caps = keycode(&keymap, xkb::keysyms::KEY_Caps_Lock.into());
        let num = keycode(&keymap, xkb::keysyms::KEY_Num_Lock.into());
        let a = keycode(&keymap, xkb::Keysym::from_char('a'));
        let keypad_one = keycode(&keymap, xkb::keysyms::KEY_KP_1.into());
        let mut model = model(&keymap);
        let mut transitions = Vec::new();
        for (keycode, state) in [
            (caps, InputState::Pressed),
            (caps, InputState::Released),
            (a, InputState::Pressed),
            (a, InputState::Released),
            (num, InputState::Pressed),
            (num, InputState::Released),
            (keypad_one, InputState::Pressed),
            (keypad_one, InputState::Released),
        ] {
            transitions.extend(
                model
                    .plan(
                        PreparedKeyboardAction::Key { keycode, state },
                        shift,
                        keymap.shift_mask,
                    )
                    .expect("plan key transition"),
            );
        }

        assert_eq!(
            pressed_symbols("gb", &transitions),
            vec![
                xkb::keysyms::KEY_Caps_Lock.into(),
                xkb::Keysym::from_char('A'),
                xkb::keysyms::KEY_Num_Lock.into(),
                xkb::keysyms::KEY_KP_1.into(),
            ]
        );
    }

    #[test]
    fn text_delivers_exact_tabs_and_supported_layout_characters() {
        let keymap = compile_keymap("gb").expect("compile keymap");
        let mut model = model(&keymap);
        let strokes = "\t£@"
            .chars()
            .map(|character| keymap.strokes[&text_keysym(character).raw()])
            .collect();

        let transitions = model
            .plan(
                PreparedKeyboardAction::Text(strokes),
                keymap.shift_keycode,
                keymap.shift_mask,
            )
            .expect("plan supported text");

        assert_eq!(pressed_text("gb", &transitions), "\t£@");
    }

    #[test]
    fn every_indexed_text_character_reaches_the_consumer_exactly() {
        for layout in ["gb", "us"] {
            let keymap = compile_keymap(layout).expect("compile keymap");
            for (&raw_keysym, &stroke) in &keymap.strokes {
                let keysym = xkb::Keysym::new(raw_keysym);
                let Some(character) = keysym.key_char() else {
                    continue;
                };
                if text_keysym(character) != keysym {
                    continue;
                }
                let mut model = model(&keymap);
                let transitions = model
                    .plan(
                        PreparedKeyboardAction::Text(vec![stroke]),
                        keymap.shift_keycode,
                        keymap.shift_mask,
                    )
                    .expect("plan indexed text character");

                assert_eq!(
                    pressed_text(layout, &transitions),
                    character.to_string(),
                    "layout {layout} did not preserve {character:?}"
                );
            }
        }
    }

    #[test]
    fn non_shift_text_modifiers_are_rejected_before_planning_events() {
        let keymap = compile_keymap("gb").expect("compile keymap");
        let mut model = model(&keymap);
        let unsupported_mask = if keymap.shift_mask == 1 { 2 } else { 1 };

        let error = model
            .plan(
                PreparedKeyboardAction::Text(vec![KeyStroke {
                    keycode: keycode(&keymap, xkb::Keysym::from_char('a')),
                    modifiers: unsupported_mask,
                }]),
                keymap.shift_keycode,
                keymap.shift_mask,
            )
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert!(model.held_keys.is_empty());
    }

    #[test]
    fn teardown_clears_locked_modifier_state() {
        let keymap = compile_keymap("gb").expect("compile keymap");
        let mut model = model(&keymap);
        let caps = keycode(&keymap, xkb::keysyms::KEY_Caps_Lock.into());
        for state in [InputState::Pressed, InputState::Released] {
            model
                .plan(
                    PreparedKeyboardAction::Key {
                        keycode: caps,
                        state,
                    },
                    keymap.shift_keycode,
                    keymap.shift_mask,
                )
                .expect("plan Caps Lock transition");
        }

        assert!(model.has_active_modifiers());
        assert_eq!(
            model.clear_modifier_state(),
            Some(KeyboardTransition::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            })
        );
        assert!(!model.has_active_modifiers());
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

    fn model(keymap: &CompiledKeymap) -> KeyboardModel {
        KeyboardModel::new(
            keymap.modifiers_by_keycode.clone(),
            keymap.locking_modifiers_by_keycode.clone(),
        )
    }

    fn keycode(keymap: &CompiledKeymap, keysym: xkb::Keysym) -> u32 {
        keymap.named_keycodes[&keysym.raw()]
    }

    fn key_events(transitions: &[KeyboardTransition]) -> Vec<(u32, InputState)> {
        transitions
            .iter()
            .filter_map(|transition| match transition {
                KeyboardTransition::Key { keycode, state } => Some((*keycode, *state)),
                KeyboardTransition::Modifiers { .. } => None,
            })
            .collect()
    }

    fn pressed_symbols(layout: &str, transitions: &[KeyboardTransition]) -> Vec<xkb::Keysym> {
        consume(layout, transitions)
            .into_iter()
            .map(|(keysym, _)| keysym)
            .collect()
    }

    fn pressed_text(layout: &str, transitions: &[KeyboardTransition]) -> String {
        consume(layout, transitions)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    fn consume(layout: &str, transitions: &[KeyboardTransition]) -> Vec<(xkb::Keysym, String)> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap =
            xkb::Keymap::new_from_names(&context, "", "", layout, "", None, xkb::COMPILE_NO_FLAGS)
                .expect("compile consumer keymap");
        let mut state = xkb::State::new(&keymap);
        let mut pressed = Vec::new();
        for transition in transitions {
            match *transition {
                KeyboardTransition::Key {
                    keycode,
                    state: InputState::Pressed,
                } => {
                    let keycode = xkb::Keycode::new(keycode + XKB_EVDEV_OFFSET);
                    pressed.push((state.key_get_one_sym(keycode), state.key_get_utf8(keycode)));
                }
                KeyboardTransition::Key { .. } => {}
                KeyboardTransition::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    state.update_mask(depressed, latched, locked, 0, 0, group);
                }
            }
        }
        pressed
    }
}
