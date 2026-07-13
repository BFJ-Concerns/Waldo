use waldo_niri::NiriInspector;
use waldo_protocol::WindowId;

#[test]
fn active_niri_session_advertises_the_required_protocols() {
    if std::env::var_os("NIRI_SOCKET").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }

    let focused = niri_ipc::socket::Socket::connect()
        .unwrap()
        .send(niri_ipc::Request::FocusedWindow)
        .unwrap()
        .unwrap();
    let niri_ipc::Response::FocusedWindow(focused) = focused else {
        panic!("Niri returned the wrong focused-window response")
    };
    let expected_focus = focused.map(|window| WindowId(window.id.to_string()));
    let snapshot = NiriInspector.inspect(false).unwrap();
    assert!(!snapshot.outputs.is_empty());
    assert!(snapshot.profile.niri_version.is_some());
    assert_eq!(snapshot.focused_window, expected_focus);
    assert!(snapshot.windows.is_empty());
    for required in [
        "zwlr_screencopy_manager_v1",
        "zwlr_virtual_pointer_manager_v1",
        "zwp_virtual_keyboard_manager_v1",
        "zwlr_data_control_manager_v1",
        "zwlr_layer_shell_v1",
    ] {
        assert!(
            snapshot
                .profile
                .protocols
                .iter()
                .any(|protocol| protocol.interface == required),
            "Niri did not advertise {required}"
        );
    }
}
