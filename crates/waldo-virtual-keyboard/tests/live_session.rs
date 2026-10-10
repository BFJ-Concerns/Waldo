//! Pinned live test: inside a Niri session the compositor's own keymap is
//! obtained, compiles, and a virtual keyboard initialises against it.
//! Elsewhere each test returns early, printing why, and the test runner then
//! counts it as passed: a green run outside a Niri session exercised nothing.

use std::sync::Arc;

use waldo_protocol::{ErrorCode, ProtocolError};
use waldo_virtual_keyboard::{ActiveLayout, KeymapOrigin, VirtualKeyboardExecutor};

fn live_session() -> bool {
    if std::env::var_os("NIRI_SOCKET").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("skipped: no live Niri session (NIRI_SOCKET and WAYLAND_DISPLAY needed)");
        return false;
    }
    true
}

#[test]
fn the_compositor_keymap_is_the_one_typed_with() {
    if !live_session() {
        return;
    }
    let executor = VirtualKeyboardExecutor::initialise(Arc::new(|| {
        Ok(ActiveLayout {
            index: 0,
            name: None,
        })
    }))
    .expect("the live seat exposes a keyboard whose keymap compiles");
    assert_eq!(executor.keymap_origin(), &KeymapOrigin::Compositor);
    assert!(
        !executor.layouts().is_empty(),
        "the compositor keymap names its layouts"
    );
}

#[test]
fn an_active_layout_outside_the_keymap_is_a_distinct_mismatch_report() {
    if !live_session() {
        return;
    }
    let unavailable = match VirtualKeyboardExecutor::initialise(Arc::new(|| {
        Ok(ActiveLayout {
            index: 99,
            name: Some("Imaginary".into()),
        })
    })) {
        Ok(_) => panic!("a group the keymap lacks is reported, not clamped"),
        Err(unavailable) => unavailable,
    };
    assert_eq!(unavailable.error.code, ErrorCode::UnsupportedCapability);
    assert!(
        unavailable.limitation.contains("Imaginary"),
        "{}",
        unavailable.limitation
    );
    let _: ProtocolError = unavailable.error;
}

#[test]
fn destructive_control_text_is_refused_before_live_keyboard_dispatch() {
    if !live_session() {
        return;
    }
    use waldo_action::{ActionExecutionFailure, ActionExecutor};
    use waldo_protocol::{Action, TextRoutePreference};
    let executor = VirtualKeyboardExecutor::initialise(Arc::new(|| {
        Ok(ActiveLayout {
            index: 0,
            name: None,
        })
    }))
    .unwrap();
    let mut controller = waldo_controller::Controller::new(std::time::Duration::from_secs(30));
    let connection = controller.connect();
    let authority = controller
        .begin_execution(connection, std::time::Duration::ZERO)
        .unwrap();
    for character in ['\u{8}', '\u{1b}', '\u{7f}'] {
        let failure = executor
            .execute(
                &Action::Text {
                    text: format!("a{character}"),
                    route: TextRoutePreference::VirtualKeyboard,
                },
                authority.cancellation(),
            )
            .unwrap_err();
        assert!(matches!(
            failure,
            ActionExecutionFailure::NotExecuted { .. }
        ));
        assert_eq!(failure.error().code, ErrorCode::InvalidRequest);
        assert!(
            failure
                .error()
                .message
                .contains(&format!("U+{:04X}", u32::from(character)))
        );
    }
}

#[test]
fn live_text_and_held_key_cleanup_report_no_limitation_on_a_healthy_connection() {
    if !live_session() {
        return;
    }
    use waldo_action::ActionExecutor;
    use waldo_protocol::{Action, InputState, TextRoutePreference};
    let executor = VirtualKeyboardExecutor::initialise(Arc::new(|| {
        Ok(ActiveLayout {
            index: 0,
            name: None,
        })
    }))
    .unwrap();
    let mut controller = waldo_controller::Controller::new(std::time::Duration::from_secs(30));
    let connection = controller.connect();
    let authority = controller
        .begin_execution(connection, std::time::Duration::ZERO)
        .unwrap();
    executor
        .execute(
            &Action::Text {
                text: "Waldo".into(),
                route: TextRoutePreference::VirtualKeyboard,
            },
            authority.cancellation(),
        )
        .unwrap();
    executor
        .execute(
            &Action::Key {
                key: "shift".into(),
                state: InputState::Pressed,
            },
            authority.cancellation(),
        )
        .unwrap();
    assert_eq!(executor.release_all(), None);
}
