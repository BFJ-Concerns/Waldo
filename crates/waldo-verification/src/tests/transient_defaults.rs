use super::*;

#[test]
fn portable_transient_fallback_honours_tmpdir() {
    const MARKER: &str = "WALDO_TEMP_DEFAULT_CHILD";
    if std::env::var_os(MARKER).is_some() {
        let parent = std::env::temp_dir();
        let mut directory =
            TransientDirectory::create_with_shared_memory(7, &parent.join("absent-shared-memory"))
                .unwrap();
        assert_eq!(directory.0.parent().unwrap(), parent);
        assert!(directory.0.is_dir());
        directory.remove().unwrap();
        assert!(!directory.0.exists());
        return;
    }
    let parent = std::env::temp_dir().join(format!("waldo-temp-default-{}", std::process::id()));
    fs::create_dir(&parent).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "clipboard_outside_in::transient_defaults::portable_transient_fallback_honours_tmpdir",
            "--nocapture",
        ])
        .env("TMPDIR", &parent)
        .env(MARKER, "1")
        .output()
        .unwrap();
    fs::remove_dir(&parent).unwrap();
    assert!(
        output.status.success(),
        "child output:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn transient_directory_prefers_available_shared_memory() {
    let parent = std::env::temp_dir().join(format!("waldo-shared-default-{}", std::process::id()));
    fs::create_dir(&parent).unwrap();
    let mut directory = TransientDirectory::create_with_shared_memory(8, &parent).unwrap();
    assert_eq!(directory.0.parent().unwrap(), parent);
    directory.remove().unwrap();
    fs::remove_dir(parent).unwrap();
}
