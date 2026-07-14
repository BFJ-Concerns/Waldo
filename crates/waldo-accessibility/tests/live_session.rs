use waldo_accessibility::AccessibilityBackend;

#[test]
fn accessibility_backend_connects_to_the_live_session_bus() {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return;
    }

    AccessibilityBackend::initialise().expect("live AT-SPI backend should initialise");
}
