//! Wayland virtual-keyboard execution with the session's actual keymap.
//!
//! The keymap is the one the compositor reports through the seat's keyboard,
//! so what Waldo types is what the operator's own keys would type. Where the
//! compositor reports none, the keymap the operator's XKB configuration
//! (`XKB_DEFAULT_LAYOUT` and friends) compiles to is used instead; a
//! session that offers neither gets a distinct unsupported report, never a
//! guess. The active layout group is read from the compositor before every
//! action, so a layout switched mid-session is honoured or the mismatch is
//! reported.

mod keymap;
mod model;
mod names;
mod wayland;

use std::{env, fmt, sync::Arc, sync::Mutex};

use crate::{
    keymap::{CompiledKeymap, GroupTable, KeySequence, KeymapError},
    model::{KeyboardModel, KeyboardTransition, PreparedKeyboardAction},
    names::{named_keysym, text_keysym},
    wayland::Device,
};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{
    Action, ActionRoute, ErrorCode, InputState, ProtocolError, TextRoutePreference,
};

/// The compositor's currently active layout: the group index into the
/// keymap and, where the compositor names it, the layout's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveLayout {
    pub index: u32,
    pub name: Option<String>,
}

/// Reads the active layout from the compositor; called before every
/// keyboard action.
pub type ActiveLayoutSource = Arc<dyn Fn() -> Result<ActiveLayout, ProtocolError> + Send + Sync>;

/// Where the keymap in use came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeymapOrigin {
    /// The seat's keyboard reported the compositor's keymap.
    Compositor,
    /// The operator's `XKB_DEFAULT_*` environment compiled to it.
    XkbEnvironment,
}

/// Why the keyboard route is unavailable for this session, in words Query
/// can disclose as the capability's limitation.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyboardUnavailable {
    pub limitation: String,
    pub error: ProtocolError,
}

impl fmt::Display for KeyboardUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.limitation)
    }
}

/// Executes key, chord, and supported text actions through virtual keyboard v1.
pub struct VirtualKeyboardExecutor {
    active_layout: ActiveLayoutSource,
    origin: KeymapOrigin,
    layouts: Vec<String>,
    state: Mutex<Option<KeyboardState>>,
}

impl VirtualKeyboardExecutor {
    /// Connects, establishes the keymap, uploads it, and reads the active
    /// layout once, so support is advertised only for a keyboard that can
    /// type.
    pub fn initialise(active_layout: ActiveLayoutSource) -> Result<Self, KeyboardUnavailable> {
        let (state, origin) = KeyboardState::connect(&active_layout)?;
        Ok(Self {
            active_layout,
            origin,
            layouts: state
                .keymap
                .layout_names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            state: Mutex::new(Some(state)),
        })
    }

    pub fn keymap_origin(&self) -> &KeymapOrigin {
        &self.origin
    }

    /// The layout names the keymap defines, in group order.
    pub fn layouts(&self) -> &[String] {
        &self.layouts
    }

    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut KeyboardState) -> Result<T, ActionExecutionFailure>,
    ) -> Result<T, ActionExecutionFailure> {
        let mut guard = self.state.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(wayland::backend_failure(
                "virtual-keyboard state lock is poisoned",
                false,
            ))
        })?;
        if guard.is_none() {
            let (state, _) = KeyboardState::connect(&self.active_layout)
                .map_err(|unavailable| ActionExecutionFailure::not_executed(unavailable.error))?;
            *guard = Some(state);
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
        self.with_state(|state| {
            state
                .refresh(&self.active_layout)
                .map_err(ActionExecutionFailure::not_executed)?;
            state.execute(action)
        })?;
        Ok(ActionRoute::VirtualKeyboard.into())
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::VirtualKeyboard
    }

    fn release_all(&self) -> Option<String> {
        // A poisoned lock still holds the device, and dropping it is the
        // release, so recover the guard rather than leave keys held.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        None
    }
}

struct KeyboardState {
    device: Device,
    keymap: CompiledKeymap,
    group: u32,
    model: KeyboardModel,
    /// Set when the compositor changed to a keymap that could not be
    /// adopted: the tables no longer match the compositor, so every action
    /// is refused with this until a usable keymap arrives.
    stale: Option<ProtocolError>,
}

impl KeyboardState {
    fn connect(
        active_layout: &ActiveLayoutSource,
    ) -> Result<(Self, KeymapOrigin), KeyboardUnavailable> {
        let (mut device, compositor_keymap) =
            Device::connect().map_err(|error| KeyboardUnavailable {
                limitation: format!("virtual keyboard is unavailable: {}", error.message),
                error,
            })?;
        let (keymap, origin) = match compositor_keymap {
            Some(text) => (
                CompiledKeymap::compile(&text).map_err(|error| {
                    unsupported_layout(format!("the compositor's keymap cannot be used: {error}"))
                })?,
                KeymapOrigin::Compositor,
            ),
            None => (xkb_environment_keymap()?, KeymapOrigin::XkbEnvironment),
        };
        device
            .upload_keymap(&keymap.text)
            .map_err(|error| KeyboardUnavailable {
                limitation: format!("virtual keyboard is unavailable: {}", error.message),
                error,
            })?;
        let group = active_group(active_layout, &keymap).map_err(|error| KeyboardUnavailable {
            limitation: error.message.clone(),
            error,
        })?;
        let model = KeyboardModel::new(
            keymap.modifiers_by_keycode.clone(),
            keymap.locking_modifiers_by_keycode.clone(),
            group,
        );
        Ok((
            Self {
                device,
                keymap,
                group,
                model,
                stale: None,
            },
            origin,
        ))
    }

    /// Picks up a keymap the compositor changed and the group it has
    /// active now, or reports why the keyboard no longer matches.
    fn refresh(&mut self, active_layout: &ActiveLayoutSource) -> Result<(), ProtocolError> {
        if let Some(text) = self.device.poll_keymap()? {
            let keymap = match CompiledKeymap::compile(&text) {
                Ok(keymap) => keymap,
                Err(error) => {
                    let error = unsupported_layout(format!(
                        "the compositor's new keymap cannot be used: {error}"
                    ))
                    .error;
                    self.stale = Some(error.clone());
                    return Err(error);
                }
            };
            // The old tables are the only record of what is held down, so
            // release everything through them before they are replaced.
            self.release_held();
            self.device.upload_keymap(&keymap.text)?;
            self.model = KeyboardModel::new(
                keymap.modifiers_by_keycode.clone(),
                keymap.locking_modifiers_by_keycode.clone(),
                self.group,
            );
            self.keymap = keymap;
            self.stale = None;
        }
        if let Some(error) = &self.stale {
            return Err(error.clone());
        }
        self.group = active_group(active_layout, &self.keymap)?;
        self.model.set_group(self.group);
        Ok(())
    }

    fn group_table(&self) -> &GroupTable {
        self.keymap
            .group(self.group)
            .expect("refresh keeps the group inside the keymap")
    }

    fn execute(&mut self, action: &Action) -> Result<(), ActionExecutionFailure> {
        let prepared = self
            .prepare(action)
            .map_err(ActionExecutionFailure::not_executed)?;
        let transitions = self
            .model
            .plan(prepared)
            .map_err(ActionExecutionFailure::not_executed)?;
        for transition in transitions {
            self.emit(transition);
        }
        self.device.settle().map_err(|error| {
            ActionExecutionFailure::indeterminate(ActionRoute::VirtualKeyboard, error)
        })
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
        let table = self.group_table();
        table
            .named_keycodes
            .get(&keysym.raw())
            .copied()
            .ok_or_else(|| ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "key {name:?} is absent from the active keyboard layout {:?}",
                    table.name
                ),
                retryable: false,
            })
    }

    fn resolve_character(&self, character: char) -> Result<KeySequence, ProtocolError> {
        let table = self.group_table();
        table
            .sequences
            .get(&text_keysym(character).raw())
            .cloned()
            .ok_or_else(|| ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "character {character:?} cannot be typed exactly on the active keyboard layout {:?}",
                    table.name
                ),
                retryable: false,
            })
    }

    fn resolve_text(&self, text: &str) -> Result<Vec<KeySequence>, ProtocolError> {
        let mut characters = text.chars().peekable();
        let mut sequences = Vec::with_capacity(text.chars().count());
        while let Some(mut character) = characters.next() {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
                character = '\n';
            }
            sequences.push(self.resolve_character(character)?);
        }
        Ok(sequences)
    }

    fn emit(&self, transition: KeyboardTransition) {
        match transition {
            KeyboardTransition::Key { keycode, state } => {
                self.device.key(keycode, state == InputState::Pressed);
            }
            KeyboardTransition::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => self.device.modifiers(depressed, latched, locked, group),
        }
    }
}

impl KeyboardState {
    /// Releases every key the model records as held and clears modifier
    /// and lock state on the compositor.
    fn release_held(&mut self) {
        for keycode in self.model.held_keys() {
            let transitions = self
                .model
                .plan(PreparedKeyboardAction::Key {
                    keycode,
                    state: InputState::Released,
                })
                .expect("a key release never needs text modifiers");
            for transition in transitions {
                self.emit(transition);
            }
        }
        if let Some(transition) = self.model.clear_modifier_state() {
            self.emit(transition);
        }
        let _ = self.device.settle();
    }
}

impl Drop for KeyboardState {
    fn drop(&mut self) {
        self.release_held();
    }
}

/// The keymap the operator's XKB configuration compiles to, taken only when
/// `XKB_DEFAULT_LAYOUT` is set explicitly; an unset environment is an
/// unsupported report, not a default layout.
fn xkb_environment_keymap() -> Result<CompiledKeymap, KeyboardUnavailable> {
    let layout = env::var("XKB_DEFAULT_LAYOUT")
        .ok()
        .filter(|value| !value.is_empty());
    let Some(layout) = layout else {
        return Err(unsupported_layout(
            "the compositor reported no keyboard keymap and XKB_DEFAULT_LAYOUT is not set",
        ));
    };
    CompiledKeymap::from_names("", "", "", "", None).map_err(|error| match error {
        KeymapError::Uncompilable => unsupported_layout(format!(
            "xkbcommon cannot compile the XKB_DEFAULT_* configuration (layout {layout:?})"
        )),
        other => unsupported_layout(format!(
            "the XKB_DEFAULT_* configuration (layout {layout:?}) cannot be used: {other}"
        )),
    })
}

/// The compositor's active group, checked against the keymap it must index.
fn active_group(
    active_layout: &ActiveLayoutSource,
    keymap: &CompiledKeymap,
) -> Result<u32, ProtocolError> {
    let active = active_layout()?;
    let groups = keymap.groups.len();
    if (active.index as usize) < groups {
        return Ok(active.index);
    }
    Err(ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: format!(
            "the active keyboard layout (index {}{}) is outside the {groups} layout(s) of the keymap in use; the keymap and the compositor disagree",
            active.index,
            active
                .name
                .as_deref()
                .map(|name| format!(", {name:?}"))
                .unwrap_or_default()
        ),
        retryable: true,
    })
}

fn unsupported_layout(limitation: impl Into<String>) -> KeyboardUnavailable {
    let limitation = limitation.into();
    KeyboardUnavailable {
        error: ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: limitation.clone(),
            retryable: false,
        },
        limitation,
    }
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
