use std::{
    os::unix::fs::PermissionsExt,
    sync::{Mutex, MutexGuard},
};

use super::*;
use waldo_verification::PrivateRoot;

// Keep newly written executables out of concurrent process-spawn windows.
static FAKE_PASTE: Mutex<()> = Mutex::new(());

struct FakePaste {
    root: PrivateRoot,
    executable: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl FakePaste {
    fn new() -> Self {
        let guard = FAKE_PASTE.lock().unwrap_or_else(|error| error.into_inner());
        let root = PrivateRoot::create_scratch("clipboard-inventory").unwrap();
        let executable = root.path().join("wl-paste");
        fs::write(
            &executable,
            r#"#!/bin/sh
set -eu
directory=${0%/*}
[ "$1" = --seat ]
scenario=$2
shift 2
printf '%s\n' "$*" >> "$directory/calls"
case "$1" in
    --watch)
        printf '%s\n' "$$" > "$directory/watcher-pid"
        shift
        case "$scenario" in
            exit) exit 7 ;;
            no-report) exec sleep 60 ;;
            nil|clear|data|sensitive) state=$scenario ;;
            first-state) state=nil ;;
            invalid) state=unknown ;;
            unset) state= ;;
            oversized) state=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx ;;
            *) state=data ;;
        esac
        CLIPBOARD_STATE="$state" "$@"
        if [ "$scenario" = first-state ]; then
            CLIPBOARD_STATE=data "$@"
        fi
        exec sleep 60
        ;;
    --list-types)
        case "$scenario" in
            nil|clear|first-state)
                printf 'Keine Auswahl vorhanden\n' >&2
                exit 1 ;;
            list-failure)
                printf 'Verbindung fehlgeschlagen\n' >&2
                exit 1 ;;
            english-failure)
                printf 'Nothing is copied\n' >&2
                exit 1 ;;
        esac
        printf 'text/plain\napplication/octet-stream\napplication/x-empty\n'
        ;;
    --no-newline)
        [ "$2" = --type ]
        if [ "$scenario" = mime-failure ]; then
            printf 'Lecture impossible\n' >&2
            exit 1
        fi
        case "$3" in
            text/plain) printf hello ;;
            application/octet-stream) printf '\000\377\001' ;;
            application/x-empty) : ;;
            *) exit 8 ;;
        esac
        ;;
    *) exit 9 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            executable,
            _guard: guard,
        }
    }

    fn inventory(
        &self,
        scenario: &str,
    ) -> Result<Vec<RepresentationEvidence>, Box<dyn std::error::Error>> {
        clipboard_inventory_with(&self.executable, scenario)
    }

    fn assert_reaped(&self) {
        let pid = fs::read_to_string(self.root.path().join("watcher-pid")).unwrap();
        assert!(
            !Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "watcher {pid:?} is still alive or unreaped"
        );
    }
}

#[test]
fn empty_selection_is_localisation_independent() {
    for scenario in ["nil", "clear", "first-state"] {
        let fake = FakePaste::new();
        assert!(fake.inventory(scenario).unwrap().is_empty());
        let calls = fs::read_to_string(fake.root.path().join("calls")).unwrap();
        assert_eq!(
            calls.lines().count(),
            1,
            "empty selection must skip MIME reads"
        );
        assert!(calls.starts_with("--watch "));
        fake.assert_reaped();
    }
}

#[test]
fn populated_selection_preserves_mime_order_bytes_and_digests() {
    let expected = vec![
        RepresentationEvidence {
            mime_type: "text/plain".into(),
            bytes: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
        },
        RepresentationEvidence {
            mime_type: "application/octet-stream".into(),
            bytes: 3,
            sha256: "47ffa3ea45a70b8a41c2c0825df323c00a8b7a01c1ea06083cc41dddcc001123".into(),
        },
        RepresentationEvidence {
            mime_type: "application/x-empty".into(),
            bytes: 0,
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        },
    ];
    for scenario in ["data", "sensitive"] {
        let fake = FakePaste::new();
        assert_eq!(fake.inventory(scenario).unwrap(), expected);
        fake.assert_reaped();
    }
}

#[test]
fn failed_reads_are_errors_regardless_of_diagnostic_wording() {
    for (scenario, context) in [
        (
            "list-failure",
            "wl-paste --list-types failed: Verbindung fehlgeschlagen",
        ),
        (
            "english-failure",
            "wl-paste --list-types failed: Nothing is copied",
        ),
        (
            "mime-failure",
            "wl-paste failed for advertised MIME \"text/plain\"",
        ),
    ] {
        let fake = FakePaste::new();
        assert_eq!(fake.inventory(scenario).unwrap_err().to_string(), context);
        fake.assert_reaped();
    }
}

#[test]
fn watcher_exiting_without_a_state_report_is_an_error() {
    let fake = FakePaste::new();
    let error = fake.inventory("exit").unwrap_err().to_string();
    assert!(error.contains("wl-paste --watch"), "{error}");
    assert!(
        error.contains("exited") || error.contains("closed"),
        "{error}"
    );
    fake.assert_reaped();
}

#[test]
fn unknown_unset_and_oversized_states_are_errors() {
    for scenario in ["invalid", "unset", "oversized"] {
        let fake = FakePaste::new();
        let error = fake.inventory(scenario).unwrap_err().to_string();
        assert!(error.contains("wl-paste --watch"), "{error}");
        assert!(error.contains("CLIPBOARD_STATE"), "{error}");
        fake.assert_reaped();
    }
}

#[test]
fn watcher_without_a_report_times_out_and_is_reaped() {
    let fake = FakePaste::new();
    let start = Instant::now();
    let error = fake.inventory("no-report").unwrap_err().to_string();
    assert!(error.contains("wl-paste --watch timed out"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(4));
    fake.assert_reaped();
}
