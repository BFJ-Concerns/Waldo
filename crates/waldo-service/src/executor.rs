use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use waldo_accessibility::AccessibilityBackend;
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_clipboard::ClipboardExecutor;
use waldo_controller::ExecutionToken;
use waldo_niri::NiriActionExecutor;
use waldo_protocol::{Action, ActionRoute, ErrorCode, ProtocolError, TextRoutePreference};
use waldo_virtual_keyboard::{ActiveLayoutSource, KeyboardUnavailable, VirtualKeyboardExecutor};
use waldo_virtual_pointer::VirtualPointerExecutor;

pub(crate) trait PasteClipboard: ActionExecutor {
    type Snapshot;
    fn snapshot(&self) -> Result<Self::Snapshot, ProtocolError>;
    fn offer_text_once(&self, text: &str) -> Result<(), ActionExecutionFailure>;
    fn temporary_paste_delivered(&self) -> Result<bool, ProtocolError>;
    fn restore_temporary(&self, snapshot: Self::Snapshot) -> Result<(), ProtocolError>;
    fn clipboard_limitation(&self) -> Option<String>;
}

impl PasteClipboard for ClipboardExecutor {
    fn clipboard_limitation(&self) -> Option<String> {
        self.limitation()
    }
    type Snapshot = waldo_clipboard::ClipboardSnapshot;
    fn snapshot(&self) -> Result<Self::Snapshot, ProtocolError> {
        self.snapshot()
    }
    fn offer_text_once(&self, text: &str) -> Result<(), ActionExecutionFailure> {
        self.offer_text_once(text)
    }
    fn temporary_paste_delivered(&self) -> Result<bool, ProtocolError> {
        self.temporary_paste_delivered()
    }
    fn restore_temporary(&self, snapshot: Self::Snapshot) -> Result<(), ProtocolError> {
        self.restore_temporary(snapshot)
    }
}

pub(crate) trait PasteKeyboard: ActionExecutor + Sized {
    fn initialise(active_layout: ActiveLayoutSource) -> Result<Self, KeyboardUnavailable>;
}

impl PasteKeyboard for VirtualKeyboardExecutor {
    fn initialise(active_layout: ActiveLayoutSource) -> Result<Self, KeyboardUnavailable> {
        Self::initialise(active_layout)
    }
}

/// Routes each action to the one live backend which owns its semantics.
pub(crate) struct DesktopActionExecutor<C = ClipboardExecutor, K = VirtualKeyboardExecutor> {
    niri: NiriActionExecutor,
    /// The pointer, or why it failed to initialise.
    pointer: Result<VirtualPointerExecutor, ProtocolError>,
    /// The keyboard, or why there is none. A retryable failure (Niri not
    /// answering at start-up, say) is retried on the next use, so a
    /// transient fault at daemon start does not disable the route for the
    /// daemon's life.
    keyboard: Mutex<Result<K, KeyboardUnavailable>>,
    active_layout: ActiveLayoutSource,
    /// The clipboard, or why it failed to initialise.
    clipboard: Result<C, ProtocolError>,
    paste_chord: Vec<String>,
    accessibility: Option<Arc<AccessibilityBackend>>,
}

impl DesktopActionExecutor {
    pub(crate) fn initialise(
        accessibility: Option<Arc<AccessibilityBackend>>,
        active_layout: ActiveLayoutSource,
        settings: &waldo_settings::Settings,
    ) -> Result<Self, waldo_settings::SettingsError> {
        Self::with_backends(
            accessibility,
            Arc::clone(&active_layout),
            settings,
            VirtualPointerExecutor::initialise(),
            VirtualKeyboardExecutor::initialise(active_layout),
            ClipboardExecutor::initialise(),
        )
    }
}

impl<C: PasteClipboard, K: PasteKeyboard> DesktopActionExecutor<C, K> {
    fn with_backends(
        accessibility: Option<Arc<AccessibilityBackend>>,
        active_layout: ActiveLayoutSource,
        settings: &waldo_settings::Settings,
        pointer: Result<VirtualPointerExecutor, ProtocolError>,
        keyboard: Result<K, KeyboardUnavailable>,
        clipboard: Result<C, ProtocolError>,
    ) -> Result<Self, waldo_settings::SettingsError> {
        Ok(Self {
            niri: NiriActionExecutor,
            pointer,
            keyboard: Mutex::new(keyboard),
            active_layout,
            paste_chord: settings.clipboard_paste_chord()?,
            clipboard,
            accessibility,
        })
    }

    /// Runs `operation` on the keyboard, first retrying initialisation when
    /// the recorded failure said a retry could succeed.
    fn with_keyboard<T>(&self, operation: impl FnOnce(&K) -> T) -> Result<T, KeyboardUnavailable> {
        let mut keyboard = self
            .keyboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(unavailable) = &*keyboard
            && unavailable.error.retryable
        {
            *keyboard = K::initialise(Arc::clone(&self.active_layout));
        }
        keyboard.as_ref().map(operation).map_err(Clone::clone)
    }

    fn keyboard_limitation(&self) -> Option<String> {
        self.with_keyboard(|_| ())
            .err()
            .map(|unavailable| unavailable.limitation)
    }

    fn keyboard_execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.with_keyboard(|keyboard| keyboard.execute(action, cancellation))
            .map_err(|unavailable| {
                ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::UnsupportedCapability,
                    message: format!(
                        "Wayland virtual-keyboard backend is unavailable: {}",
                        unavailable.limitation
                    ),
                    retryable: unavailable.error.retryable,
                })
            })?
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
        self.keyboard_execute(
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
            .map_err(|error| unavailable_backend("clipboard", error))?;
        self.with_keyboard(|_| ()).map_err(|unavailable| {
            unsupported(&format!(
                "clipboard paste needs the virtual keyboard: {}",
                unavailable.limitation
            ))
        })?;
        let snapshot = clipboard
            .snapshot()
            .map_err(ActionExecutionFailure::not_executed)?;
        if let Err(failure) = clipboard.offer_text_once(text) {
            return restore_after_failure(clipboard, snapshot, failure);
        }
        let key_result = self.keyboard_execute(
            &Action::KeyChord {
                keys: self.paste_chord.clone(),
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
            match clipboard.temporary_paste_delivered() {
                Ok(true) => break,
                Ok(false) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(false) => {
                    let failure = ActionExecutionFailure::indeterminate(
                        ActionRoute::ClipboardPaste,
                        ProtocolError {
                            code: ErrorCode::BackendFailure,
                            message: "application did not take the temporary paste text before the paste deadline".into(),
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

impl<C: PasteClipboard, K: PasteKeyboard> ActionExecutor for DesktopActionExecutor<C, K> {
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
                .map_err(|error| unavailable_backend("virtual-pointer", error))?
                .execute(action, cancellation),
            Action::Key { .. } | Action::KeyChord { .. } => {
                self.keyboard_execute(action, cancellation)
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
                .map_err(|error| unavailable_backend("clipboard", error))?
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
            ActionRoute::VirtualPointer => self
                .pointer
                .as_ref()
                .is_ok_and(|pointer| pointer.supports(route)),
            ActionRoute::VirtualKeyboard => self
                .with_keyboard(|keyboard| keyboard.supports(route))
                .unwrap_or(false),
            ActionRoute::Clipboard => self
                .clipboard
                .as_ref()
                .is_ok_and(|clipboard| clipboard.supports(route)),
            _ => false,
        }
    }

    fn limitation(&self, route: ActionRoute) -> Option<String> {
        match route {
            ActionRoute::VirtualPointer => self
                .pointer
                .as_ref()
                .err()
                .map(|error| initialisation_failed("virtual-pointer", error)),
            ActionRoute::VirtualKeyboard => self.keyboard_limitation(),
            ActionRoute::Clipboard => match &self.clipboard {
                Ok(clipboard) => clipboard.clipboard_limitation(),
                Err(error) => Some(initialisation_failed("clipboard", error)),
            },
            _ => None,
        }
    }

    fn caveat(&self, route: ActionRoute) -> Option<String> {
        self.clipboard
            .as_ref()
            .ok()
            .and_then(|clipboard| clipboard.caveat(route))
    }

    fn release_all(&self) -> Option<String> {
        if let Ok(pointer) = &self.pointer {
            pointer.release_all();
        }
        if let Ok(keyboard) = &*self
            .keyboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            keyboard.release_all();
        }
        self.clipboard
            .as_ref()
            .ok()
            .and_then(|clipboard| clipboard.release_all())
    }
}

fn initialisation_failed(backend: &str, error: &ProtocolError) -> String {
    format!(
        "Wayland {backend} backend initialisation failed: {}",
        error.message
    )
}

/// A backend that failed to initialise stays unavailable until `waldod`
/// restarts, so repeating the action cannot succeed.
fn unavailable_backend(backend: &str, error: &ProtocolError) -> ActionExecutionFailure {
    ActionExecutionFailure::not_executed(ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: initialisation_failed(backend, error),
        retryable: false,
    })
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

/// Restores the operator's clipboard after a failed paste. When restoration
/// fails too, the clipboard has been mutated, so the result is indeterminate,
/// but it keeps the paste failure's code and message ahead of the restoration
/// failure so the caller can still tell what happened to the text.
fn restore_after_failure<C: PasteClipboard>(
    clipboard: &C,
    snapshot: C::Snapshot,
    failure: ActionExecutionFailure,
) -> Result<ActionExecution, ActionExecutionFailure> {
    let failure = as_clipboard_paste_failure(failure);
    match clipboard.restore_temporary(snapshot) {
        Ok(()) => Err(failure),
        Err(restoration) => {
            let original = failure.error();
            Err(ActionExecutionFailure::indeterminate(
                ActionRoute::ClipboardPaste,
                ProtocolError {
                    code: original.code,
                    message: format!(
                        "{}; restoring the clipboard afterwards also failed: {}",
                        original.message, restoration.message
                    ),
                    retryable: false,
                },
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use waldo_action::{ActionExecution, ActionExecutionFailure};
    use waldo_protocol::{ActionRoute, ErrorCode, ProtocolError};

    use super::automatic_text;

    #[derive(Default)]
    struct TestClipboard {
        restore_failure: Option<ProtocolError>,
    }

    impl waldo_action::ActionExecutor for TestClipboard {
        fn execute(
            &self,
            _: &waldo_protocol::Action,
            _: &waldo_controller::ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            panic!("clipboard paste must use its transport operations")
        }
    }

    impl super::PasteClipboard for TestClipboard {
        type Snapshot = ();
        fn snapshot(&self) -> Result<(), ProtocolError> {
            Ok(())
        }
        fn offer_text_once(&self, _: &str) -> Result<(), ActionExecutionFailure> {
            Ok(())
        }
        fn temporary_paste_delivered(&self) -> Result<bool, ProtocolError> {
            Ok(true)
        }
        fn restore_temporary(&self, _: ()) -> Result<(), ProtocolError> {
            self.restore_failure.clone().map_or(Ok(()), Err)
        }
        fn clipboard_limitation(&self) -> Option<String> {
            None
        }
    }

    struct TestKeyboard {
        observed: std::sync::Arc<std::sync::Mutex<Vec<waldo_protocol::Action>>>,
        refusal: Option<ActionExecutionFailure>,
    }

    impl waldo_action::ActionExecutor for TestKeyboard {
        fn execute(
            &self,
            action: &waldo_protocol::Action,
            _: &waldo_controller::ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            self.observed.lock().unwrap().push(action.clone());
            match &self.refusal {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(ActionRoute::VirtualKeyboard.into()),
            }
        }
    }

    impl super::PasteKeyboard for TestKeyboard {
        fn initialise(
            _: waldo_virtual_keyboard::ActiveLayoutSource,
        ) -> Result<Self, waldo_virtual_keyboard::KeyboardUnavailable> {
            panic!("the injected keyboard is already available")
        }
    }

    #[test]
    fn configured_paste_chord_reaches_keyboard_and_refusal_propagates() {
        use waldo_action::ActionExecutor;
        let settings = waldo_settings::Settings::from_sources(
            Some((
                std::path::Path::new("settings"),
                "WALDO_CLIPBOARD_PASTE_CHORD=ctrl+v",
            )),
            [("WALDO_CLIPBOARD_PASTE_CHORD".into(), "shift+Insert".into())],
            None,
        )
        .unwrap();
        let mut controller = waldo_controller::Controller::new(std::time::Duration::from_secs(30));
        let connection = controller.connect();
        let authority = controller
            .begin_execution(connection, std::time::Duration::ZERO)
            .unwrap();
        let refusal = ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: "key Insert is absent from active layout".into(),
            retryable: false,
        });
        for failure in [None, Some(refusal.clone())] {
            let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let executor = super::DesktopActionExecutor::with_backends(
                None,
                std::sync::Arc::new(|| panic!("the injected keyboard needs no layout discovery")),
                &settings,
                Err(ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: "the paste test needs no pointer".into(),
                    retryable: false,
                }),
                Ok(TestKeyboard {
                    observed: observed.clone(),
                    refusal: failure.clone(),
                }),
                Ok(TestClipboard::default()),
            )
            .unwrap();
            let result = executor.execute(
                &waldo_protocol::Action::Text {
                    text: "paste me".into(),
                    route: waldo_protocol::TextRoutePreference::ClipboardPaste,
                },
                authority.cancellation(),
            );
            assert_eq!(
                *observed.lock().unwrap(),
                vec![waldo_protocol::Action::KeyChord {
                    keys: vec!["shift".into(), "Insert".into()]
                }]
            );
            assert_eq!(
                result,
                match failure {
                    Some(error) => Err(error),
                    None => Ok(ActionRoute::ClipboardPaste.into()),
                }
            );
        }
    }

    #[test]
    fn a_failed_restoration_keeps_the_paste_failure_it_followed() {
        use waldo_action::ActionExecutor;
        let settings = waldo_settings::Settings::from_sources(None, [], None).unwrap();
        let mut controller = waldo_controller::Controller::new(std::time::Duration::from_secs(30));
        let connection = controller.connect();
        let authority = controller
            .begin_execution(connection, std::time::Duration::ZERO)
            .unwrap();
        let executor = super::DesktopActionExecutor::with_backends(
            None,
            std::sync::Arc::new(|| panic!("the injected keyboard needs no layout discovery")),
            &settings,
            Err(ProtocolError {
                code: ErrorCode::BackendFailure,
                message: "the paste test needs no pointer".into(),
                retryable: false,
            }),
            Ok(TestKeyboard {
                observed: Default::default(),
                refusal: Some(ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::UnsupportedCapability,
                    message: "key Insert is absent from active layout".into(),
                    retryable: false,
                })),
            }),
            Ok(TestClipboard {
                restore_failure: Some(ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: "selection owner vanished".into(),
                    retryable: true,
                }),
            }),
        )
        .unwrap();

        let result = executor.execute(
            &waldo_protocol::Action::Text {
                text: "paste me".into(),
                route: waldo_protocol::TextRoutePreference::ClipboardPaste,
            },
            authority.cancellation(),
        );

        let Err(ActionExecutionFailure::Indeterminate { route, error }) = result else {
            panic!("an unrestored clipboard must make the paste indeterminate: {result:?}")
        };
        assert_eq!(route, ActionRoute::ClipboardPaste);
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert!(error.message.contains("absent from active layout"));
        assert!(error.message.contains("selection owner vanished"));
    }

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
