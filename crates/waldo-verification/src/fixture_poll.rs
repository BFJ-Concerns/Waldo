//! Waiting for a fixture to publish a JSON state that satisfies a check.

use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;

/// Polls `path` until its JSON satisfies `predicate`. A missing file, or
/// one caught mid-write, is waited out; the last read or parse error is
/// kept and reported if the wait times out, and any other read failure,
/// such as a permission error, ends the wait at once.
pub(crate) fn poll_json<T: DeserializeOwned>(
    path: &Path,
    timeout: Duration,
    interval: Duration,
    predicate: impl Fn(&T) -> bool,
) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    loop {
        // Why the latest attempt did not succeed, reported on timeout.
        let last_error = match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                Ok(state) if predicate(&state) => return Ok(state),
                Ok(_) => None,
                Err(error) => Some(format!("unreadable JSON: {error}")),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some("the file does not exist".to_owned())
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {} ms waiting for {}: {}",
                timeout.as_millis(),
                path.display(),
                last_error.unwrap_or_else(|| "its state never satisfied the check".to_owned())
            ));
        }
        thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::poll_json;

    #[test]
    fn a_timeout_reports_the_last_parse_error() {
        let directory = std::env::temp_dir().join(format!("waldo-poll-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("state.json");
        std::fs::write(&path, b"{not json").unwrap();

        let error = poll_json::<serde_json::Value>(
            &path,
            Duration::from_millis(30),
            Duration::from_millis(10),
            |_| true,
        )
        .unwrap_err();

        assert!(error.contains("unreadable JSON"), "{error}");
        let _ = std::fs::remove_dir_all(directory);
    }
}
