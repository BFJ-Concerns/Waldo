//! Pinned live test: inside a Niri session the compositor's own keymap is
//! obtained, compiles, and a virtual keyboard initialises against it.
//! Skips itself elsewhere and says so; a skip is not a pass.

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
