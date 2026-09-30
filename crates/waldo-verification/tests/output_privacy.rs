use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct ScratchDirectory(PathBuf);

impl ScratchDirectory {
    fn new() -> Self {
        let path = PathBuf::from("/tmp").join(format!(
            "waldo-output-privacy-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        println!("Created scratch directory: {}", path.display());
        Self(path)
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.0) {
            Ok(()) => println!("Removed scratch directory: {}", self.0.display()),
            Err(error) => panic!("failed to remove {}: {error}", self.0.display()),
        }
    }
}

fn invoke(arguments: &[&str], umask: &str, output: Option<&Path>) -> Output {
    // Set umask in the child: it is process-wide and must not affect other tests.
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "umask \"$1\"; shift; exec \"$@\"",
            "waldo-output-privacy",
        ])
        .arg(umask)
        .arg(env!("CARGO_BIN_EXE_waldo-verification"))
        .args(arguments);
    if let Some(path) = output {
        command.arg("--output").arg(path);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "emitter failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    result
}

fn assert_private_output(arguments: &[&str], expected: &[u8], existing: bool) {
    let scratch = ScratchDirectory::new();
    for umask in ["022", "077"] {
        let path = scratch.0.join(format!("output-{umask}.json"));
        if existing {
            fs::write(&path, b"stale evidence that must be replaced\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let stdout = invoke(arguments, umask, None);
        assert_eq!(stdout.stdout, expected, "stdout JSON changed");
        assert!(stdout.stderr.is_empty());
        let written = invoke(arguments, umask, Some(&path));
        assert!(written.stdout.is_empty());
        assert!(written.stderr.is_empty());
        assert_eq!(fs::read(&path).unwrap(), expected, "file JSON changed");
        assert_eq!(fs::read(&path).unwrap(), stdout.stdout);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        println!("{}: umask {umask}, mode {mode:04o}", path.display());
        assert_eq!(mode, 0o600, "output must be owner-only under umask {umask}");
    }
}

const GRID_ARGUMENTS: &[&str] = &[
    "target-grid",
    "--x",
    "0",
    "--y",
    "0",
    "--width",
    "1",
    "--height",
    "1",
    "--columns",
    "1",
    "--rows",
    "1",
    "--seed",
    "0",
];
const GRID_JSON: &[u8] = br#"{
  "seed": 0,
  "bounds": {
    "x": 0,
    "y": 0,
    "width": 1,
    "height": 1
  },
  "columns": 1,
  "rows": 1,
  "targets": [
    {
      "id": "cell-0-0",
      "x": 0,
      "y": 0
    }
  ]
}
"#;

const BOUNDARY_ARGUMENTS: &[&str] = &[
    "boundary-targets",
    "--x",
    "0",
    "--y",
    "0",
    "--width",
    "1",
    "--height",
    "1",
];
const BOUNDARY_JSON: &[u8] = br#"{
  "bounds": {
    "x": 0,
    "y": 0,
    "width": 1,
    "height": 1
  },
  "targets": [
    {
      "id": "top-left",
      "x": 0,
      "y": 0
    },
    {
      "id": "top",
      "x": 0,
      "y": 0
    },
    {
      "id": "top-right",
      "x": 0,
      "y": 0
    },
    {
      "id": "right",
      "x": 0,
      "y": 0
    },
    {
      "id": "bottom-right",
      "x": 0,
      "y": 0
    },
    {
      "id": "bottom",
      "x": 0,
      "y": 0
    },
    {
      "id": "bottom-left",
      "x": 0,
      "y": 0
    },
    {
      "id": "left",
      "x": 0,
      "y": 0
    },
    {
      "id": "centre",
      "x": 0,
      "y": 0
    }
  ]
}
"#;

#[test]
fn target_grid_creates_private_output_with_unchanged_json() {
    assert_private_output(GRID_ARGUMENTS, GRID_JSON, false);
}

#[test]
fn target_grid_replaces_public_output_with_private_unchanged_json() {
    assert_private_output(GRID_ARGUMENTS, GRID_JSON, true);
}

#[test]
fn boundary_targets_creates_private_output_with_unchanged_json() {
    assert_private_output(BOUNDARY_ARGUMENTS, BOUNDARY_JSON, false);
}

#[test]
fn boundary_targets_replaces_public_output_with_private_unchanged_json() {
    assert_private_output(BOUNDARY_ARGUMENTS, BOUNDARY_JSON, true);
}
