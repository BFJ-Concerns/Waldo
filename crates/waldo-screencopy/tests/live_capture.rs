use waldo_screencopy::Screencopy;

#[test]
fn captures_a_png_from_the_active_niri_output() {
    if std::env::var_os("NIRI_SOCKET").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let output = niri_ipc::socket::Socket::connect()
        .unwrap()
        .send(niri_ipc::Request::FocusedOutput)
        .unwrap()
        .unwrap();
    let niri_ipc::Response::FocusedOutput(Some(output)) = output else {
        panic!("Niri has no focused output");
    };

    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let capture = Screencopy.capture_output(&output.name).unwrap();
    let completed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    assert!(capture.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(capture.width > 0);
    assert!(capture.height > 0);
    assert!((started_at..=completed_at).contains(&capture.captured_at_unix_ms));
}
