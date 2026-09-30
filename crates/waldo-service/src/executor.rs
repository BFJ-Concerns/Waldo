use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use waldo_accessibility::AccessibilityBackend;
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_clipboard::ClipboardExecutor;
use waldo_controller::ExecutionToken;
use waldo_niri::NiriActionExecutor;
use waldo_protocol::{Action, ActionRoute, ErrorCode, ProtocolError, TextRoutePreference};
use waldo_virtual_keyboard::{ActiveLayoutSource, VirtualKeyboardExecutor};
use waldo_virtual_pointer::VirtualPointerExecutor;

/// Routes each action to the one live backend which owns its semantics.
pub(crate) struct DesktopActionExecutor {
    niri: NiriActionExecutor,
    pointer: Option<VirtualPointerExecutor>,
    /// The keyboard, or the limitation Query discloses for why there is none.
    keyboard: Result<VirtualKeyboardExecutor, String>,
    clipboard: Option<ClipboardExecutor>,
    accessibility: Option<Arc<AccessibilityBackend>>,
}

impl DesktopActionExecutor {
    pub(crate) fn initialise(
        accessibility: Option<Arc<AccessibilityBackend>>,
        active_layout: ActiveLayoutSource,
    ) -> Self {
        Self {
            niri: NiriActionExecutor,
            pointer: VirtualPointerExecutor::initialise().ok(),
            keyboard: VirtualKeyboardExecutor::initialise(active_layout)
                .map_err(|unavailable| unavailable.limitation),
            clipboard: ClipboardExecutor::initialise().ok(),
            accessibility,
        }
    }

    fn keyboard(&self) -> Result<&VirtualKeyboardExecutor, ActionExecutionFailure> {
        self.keyboard.as_ref().map_err(|limitation| {
            ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!("Wayland virtual-keyboard backend is unavailable: {limitation}"),
                retryable: false,
            })
        })
    }

    fn execute_text(
        &self,
        text: &str,
        route: TextRoutePreference,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        match route {
            TextRoutePreference::Accessibility => self.accessibility_text(text, cancellation),
            TextRoutePreference::ClipboardPaste => self.clipboard_paste(text, cancellation),
            TextRoutePreference::VirtualKeyboard => self.virtual_keyboard_text(text, cancellation),
            TextRoutePreference::Automatic => automatic_text(
                || self.accessibility_text(text, cancellation),
                || self.clipboard_paste(text, cancellation),
                || self.virtual_keyboard_text(text, cancellation),
            ),
        }
    }

    fn accessibility_text(
        &self,
        text: &str,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.accessibility
            .as_ref()
            .ok_or_else(|| unsupported("AT-SPI text entry is unavailable"))?
            .set_focused_text(text, cancellation)
    }

    fn virtual_keyboard_text(
        &self,
        text: &str,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.keyboard()?.execute(
            &Action::Text {
                text: text.to_owned(),
                route: TextRoutePreference::VirtualKeyboard,
            },
            cancellation,
        )
    }

    fn clipboard_paste(
        &self,
        text: &str,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let clipboard = self
            .clipboard
            .as_ref()
            .ok_or_else(|| unsupported("Wayland clipboard paste is unavailable"))?;
        let keyboard = self.keyboard()?;
        let snapshot = clipboard
            .snapshot()
            .map_err(ActionExecutionFailure::not_executed)?;
        if let Err(failure) = clipboard.offer_text_once(text) {
            return restore_after_failure(clipboard, snapshot, failure);
        }
        let key_result = keyboard.execute(
            &Action::KeyChord {
                keys: vec!["Control".into(), "v".into()],
            },
            cancellation,
        );
        if let Err(failure) = key_result {
            return restore_after_failure(clipboard, snapshot, failure);
        }

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if cancellation.is_cancelled() {
                let failure = ActionExecutionFailure::indeterminate(
                    ActionRoute::ClipboardPaste,
                    ProtocolError {
                        code: ErrorCode::Cancelled,
                        message: "clipboard paste was cancelled after key dispatch".into(),
                        retryable: false,
                    },
                );
                return restore_after_failure(clipboard, snapshot, failure);
            }
            match clipboard.is_empty() {
                Ok(true) => break,
                Ok(false) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(false) => {
                    let failure = ActionExecutionFailure::indeterminate(
                        ActionRoute::ClipboardPaste,
                        ProtocolError {
                            code: ErrorCode::BackendFailure,
                            message: "application did not consume the temporary clipboard before the paste deadline".into(),
                            retryable: true,
                        },
                    );
                    return restore_after_failure(clipboard, snapshot, failure);
                }
                Err(error) => {
                    let failure =
                        ActionExecutionFailure::indeterminate(ActionRoute::ClipboardPaste, error);
                    return restore_after_failure(clipboard, snapshot, failure);
                }
            }
        }
        clipboard.restore_temporary(snapshot).map_err(|error| {
            ActionExecutionFailure::indeterminate(ActionRoute::ClipboardPaste, error)
        })?;
        Ok(ActionRoute::ClipboardPaste.into())
    }
}

fn automatic_text(
    accessibility: impl FnOnce() -> Result<ActionExecution, ActionExecutionFailure>,
    clipboard: impl FnOnce() -> Result<ActionExecution, ActionExecutionFailure>,
    virtual_keyboard: impl FnOnce() -> Result<ActionExecution, ActionExecutionFailure>,
) -> Result<ActionExecution, ActionExecutionFailure> {
    match accessibility() {
        Ok(execution) => return Ok(execution),
        Err(failure @ ActionExecutionFailure::Indeterminate { .. }) => return Err(failure),
        Err(ActionExecutionFailure::NotExecuted { .. }) => {}
    }
    match clipboard() {
        Ok(execution) => Ok(execution),
        Err(failure @ ActionExecutionFailure::Indeterminate { .. }) => Err(failure),
        Err(ActionExecutionFailure::NotExecuted { .. }) => virtual_keyboard(),
    }
}

impl ActionExecutor for DesktopActionExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        match action {
            Action::PointerMoveAbsolute { .. }
            | Action::PointerMoveRelative { .. }
            | Action::PointerButton { .. }
            | Action::PointerClick { .. }
            | Action::PointerDrag { .. }
            | Action::PointerPath { .. }
            | Action::Scroll { .. } => self
                .pointer
                .as_ref()
                .ok_or_else(|| {
                    ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "Wayland virtual-pointer backend is unavailable".into(),
                        retryable: true,
                    })
                })?
                .execute(action, cancellation),
            Action::Key { .. } | Action::KeyChord { .. } => {
                self.keyboard()?.execute(action, cancellation)
            }
            Action::Text { text, route } => self.execute_text(text, *route, cancellation),
            Action::ClipboardRead { .. }
            | Action::ClipboardWrite { .. }
            | Action::ClipboardWriteOffer { .. }
            | Action::ClipboardPreserve
            | Action::ClipboardRestore { .. }
            | Action::ClipboardKeep { .. } => self
                .clipboard
                .as_ref()
                .ok_or_else(|| {
                    ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "Wayland clipboard backend is unavailable".into(),
                        retryable: true,
                    })
                })?
                .execute(action, cancellation),
            Action::Accessibility { .. } => self
                .accessibility
                .as_ref()
                .ok_or_else(|| {
                    ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "AT-SPI backend is unavailable".into(),
                        retryable: true,
                    })
                })?
                .execute(action, cancellation),
            _ => self.niri.execute(action, cancellation),
        }
    }

    fn supports(&self, route: ActionRoute) -> bool {
        match route {
            ActionRoute::NiriIpc => self.niri.supports(route),
            ActionRoute::VirtualPointer => self
                .pointer
                .as_ref()
                .is_some_and(|pointer| pointer.supports(route)),
            ActionRoute::VirtualKeyboard => self
                .keyboard
                .as_ref()
                .is_ok_and(|keyboard| keyboard.supports(route)),
            ActionRoute::Clipboard => self
                .clipboard
                .as_ref()
                .is_some_and(|clipboard| clipboard.supports(route)),
            ActionRoute::ClipboardPaste => self.clipboard.is_some() && self.keyboard.is_ok(),
            ActionRoute::Accessibility => self
                .accessibility
                .as_ref()
                .is_some_and(|accessibility| accessibility.supports(route)),
            _ => false,
        }
    }

    fn limitation(&self, route: ActionRoute) -> Option<String> {
        match route {
            ActionRoute::VirtualKeyboard => self.keyboard.as_ref().err().cloned(),
            ActionRoute::Clipboard => self
                .clipboard
                .as_ref()
                .and_then(|clipboard| clipboard.limitation()),
            ActionRoute::ClipboardPaste => self
                .clipboard
                .as_ref()
                .and_then(|clipboard| clipboard.limitation())
                .or_else(|| {
                    self.keyboard.as_ref().err().map(|limitation| {
                        format!("clipboard paste needs the keyboard: {limitation}")
                    })
                }),
            _ => None,
        }
    }

    fn release_all(&self) {
        if let Some(pointer) = &self.pointer {
            pointer.release_all();
        }
        if let Ok(keyboard) = &self.keyboard {
            keyboard.release_all();
        }
        if let Some(clipboard) = &self.clipboard {
            clipboard.cleanup_preservation();
        }
    }
}

fn unsupported(message: &str) -> ActionExecutionFailure {
    ActionExecutionFailure::not_executed(ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: message.into(),
        retryable: false,
    })
}

fn as_clipboard_paste_failure(failure: ActionExecutionFailure) -> ActionExecutionFailure {
    match failure {
        ActionExecutionFailure::NotExecuted { error } => {
            ActionExecutionFailure::NotExecuted { error }
        }
        ActionExecutionFailure::Indeterminate { error, .. } => {
            ActionExecutionFailure::indeterminate(ActionRoute::ClipboardPaste, error)
        }
    }
}

fn restore_after_failure(
    clipboard: &ClipboardExecutor,
    snapshot: waldo_clipboard::ClipboardSnapshot,
    failure: ActionExecutionFailure,
) -> Result<ActionExecution, ActionExecutionFailure> {
    clipboard.restore_temporary(snapshot).map_err(|error| {
        ActionExecutionFailure::indeterminate(ActionRoute::ClipboardPaste, error)
    })?;
    Err(as_clipboard_paste_failure(failure))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use waldo_action::{ActionExecution, ActionExecutionFailure};
    use waldo_protocol::{ActionRoute, ErrorCode, ProtocolError};

    use super::automatic_text;

    #[test]
    fn automatic_never_falls_through_an_indeterminate_route() {
        let clipboard_attempted = Cell::new(false);
        let keyboard_attempted = Cell::new(false);
        let result = automatic_text(
            || indeterminate(ActionRoute::Accessibility),
            || {
                clipboard_attempted.set(true);
                executed(ActionRoute::ClipboardPaste)
            },
            || {
                keyboard_attempted.set(true);
                executed(ActionRoute::VirtualKeyboard)
            },
        );
        assert!(matches!(
            result,
            Err(ActionExecutionFailure::Indeterminate {
                route: ActionRoute::Accessibility,
                ..
            })
        ));
        assert!(!clipboard_attempted.get());
        assert!(!keyboard_attempted.get());

        let keyboard_attempted = Cell::new(false);
        let result = automatic_text(
            not_executed,
            || indeterminate(ActionRoute::ClipboardPaste),
            || {
                keyboard_attempted.set(true);
                executed(ActionRoute::VirtualKeyboard)
            },
        );
        assert!(matches!(
            result,
            Err(ActionExecutionFailure::Indeterminate {
                route: ActionRoute::ClipboardPaste,
                ..
            })
        ));
        assert!(!keyboard_attempted.get());
    }

    #[test]
    fn automatic_stops_after_an_acknowledged_clipboard_transport() {
        let keyboard_attempted = Cell::new(false);
        let result = automatic_text(
            not_executed,
            || executed(ActionRoute::ClipboardPaste),
            || {
                keyboard_attempted.set(true);
                executed(ActionRoute::VirtualKeyboard)
            },
        )
        .unwrap();

        assert_eq!(result.route, ActionRoute::ClipboardPaste);
        assert!(!keyboard_attempted.get());
    }

    #[test]
    fn automatic_falls_through_only_definitely_not_executed_routes() {
        let result = automatic_text(not_executed, not_executed, || {
            executed(ActionRoute::VirtualKeyboard)
        })
        .unwrap();

        assert_eq!(result.route, ActionRoute::VirtualKeyboard);
    }

    fn executed(route: ActionRoute) -> Result<ActionExecution, ActionExecutionFailure> {
        Ok(route.into())
    }

    fn not_executed() -> Result<ActionExecution, ActionExecutionFailure> {
        Err(ActionExecutionFailure::not_executed(error()))
    }

    fn indeterminate(route: ActionRoute) -> Result<ActionExecution, ActionExecutionFailure> {
        Err(ActionExecutionFailure::indeterminate(route, error()))
    }

    fn error() -> ProtocolError {
        ProtocolError {
            code: ErrorCode::BackendFailure,
            message: "deliberate route boundary".into(),
            retryable: true,
        }
    }
}
