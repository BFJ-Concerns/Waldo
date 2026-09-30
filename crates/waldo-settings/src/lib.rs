//! Operator settings: the values that differ between machines or people.
//!
//! Every such value is declared once here as a [`SettingSpec`], read from one
//! precedence chain — a command-line flag where the binary offers one, then
//! the process environment, then the settings file, then the declared
//! default — and named in the README's configuration reference, which a test
//! in this crate enforces. Tuning internals (poll intervals, quiet periods,
//! deadlines, resource caps) are not settings and do not belong here.
//!
//! The settings file is `${XDG_CONFIG_HOME:-$HOME/.config}/waldo/settings`:
//! one `NAME=VALUE` per line, `#` comments, no quoting or expansion. It
//! exists so the operator sets a value once for every Waldo binary, however
//! each is launched — the controller under systemd, the MCP server under the
//! agent, the CLI from a shell — without editing a service file or a plugin
//! manifest. A name the file carries that no binary declares is an error,
//! so a misspelt setting never silently does nothing.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

/// One operator-settable value: its environment-variable name and the
/// default the README documents for it (`None` when the default is derived,
/// such as a path beneath a runtime directory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingSpec {
    pub name: &'static str,
    pub default: Option<&'static str>,
}

/// Path of the private controller socket both bindings and the service use.
pub const SOCKET_PATH: SettingSpec = SettingSpec {
    name: "WALDO_SOCKET_PATH",
    default: None,
};

/// Seconds without a request or running batch after which a driving lease
/// lapses. It has a default and cannot be disabled.
pub const LEASE_IDLE_TIMEOUT_SECONDS: SettingSpec = SettingSpec {
    name: "WALDO_LEASE_IDLE_TIMEOUT_SECONDS",
    default: Some("30"),
};

/// Key names dispatched by the clipboard-paste route.
pub const CLIPBOARD_PASTE_CHORD: SettingSpec = SettingSpec {
    name: "WALDO_CLIPBOARD_PASTE_CHORD",
    default: Some("ctrl+v"),
};

/// Longest edge of images delivered through MCP, in pixels.
pub const MODEL_IMAGE_MAX_EDGE: SettingSpec = SettingSpec {
    name: "WALDO_MODEL_IMAGE_MAX_EDGE",
    default: Some("2048"),
};

/// Every declared setting. A setting absent from this list is unreadable
/// through [`Settings`], and a name present here without a README row fails
/// the documentation test.
pub const ALL: &[SettingSpec] = &[
    SOCKET_PATH,
    LEASE_IDLE_TIMEOUT_SECONDS,
    CLIPBOARD_PASTE_CHORD,
    MODEL_IMAGE_MAX_EDGE,
];

/// Where a resolved value came from, for error messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Environment,
    File(PathBuf),
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment => formatter.write_str("the process environment"),
            Self::File(path) => write!(formatter, "{}", path.display()),
        }
    }
}

#[derive(Debug)]
pub enum SettingsError {
    UnreadableFile {
        path: PathBuf,
        error: io::Error,
    },
    MalformedLine {
        path: PathBuf,
        line: usize,
    },
    UnknownName {
        path: PathBuf,
        line: usize,
        name: String,
    },
    InvalidValue {
        name: &'static str,
        source: Source,
        value: String,
        reason: String,
    },
    RuntimeDirectoryMissing,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnreadableFile { path, error } => {
                write!(
                    formatter,
                    "cannot read settings file {}: {error}",
                    path.display()
                )
            }
            Self::MalformedLine { path, line } => {
                write!(formatter, "{}:{line}: expected NAME=VALUE", path.display())
            }
            Self::UnknownName { path, line, name } => write!(
                formatter,
                "{}:{line}: {name} is not a Waldo setting",
                path.display()
            ),
            Self::InvalidValue {
                name,
                source,
                value,
                reason,
            } => write!(
                formatter,
                "{name}={value:?} (from {source}) is invalid: {reason}"
            ),
            Self::RuntimeDirectoryMissing => formatter
                .write_str("XDG_RUNTIME_DIR is not set and WALDO_SOCKET_PATH names no socket"),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnreadableFile { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// The resolved values of every declared setting for this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Values are kept as the operating system gave them: a path setting
    /// need not be UTF-8.
    values: BTreeMap<&'static str, (OsString, Source)>,
    runtime_directory: Option<PathBuf>,
}

impl Settings {
    /// Reads the settings file (when present) and the process environment.
    pub fn load() -> Result<Self, SettingsError> {
        let path = settings_file_path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(SettingsError::UnreadableFile { path, error }),
        };
        Self::from_sources(
            contents
                .as_deref()
                .map(|contents| (path.as_path(), contents)),
            env::vars_os(),
            env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        )
    }

    /// Resolves from explicit sources; the environment wins over the file.
    pub fn from_sources(
        file: Option<(&Path, &str)>,
        environment: impl IntoIterator<Item = (OsString, OsString)>,
        runtime_directory: Option<PathBuf>,
    ) -> Result<Self, SettingsError> {
        let mut values = BTreeMap::new();
        if let Some((path, contents)) = file {
            for (index, raw) in contents.lines().enumerate() {
                let line = index + 1;
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let (name, value) =
                    trimmed
                        .split_once('=')
                        .ok_or_else(|| SettingsError::MalformedLine {
                            path: path.to_owned(),
                            line,
                        })?;
                let spec = declared(name.trim()).ok_or_else(|| SettingsError::UnknownName {
                    path: path.to_owned(),
                    line,
                    name: name.trim().to_owned(),
                })?;
                values.insert(
                    spec.name,
                    (OsString::from(value.trim()), Source::File(path.to_owned())),
                );
            }
        }
        for (name, value) in environment {
            if let Some(spec) = name.to_str().and_then(declared) {
                values.insert(spec.name, (value, Source::Environment));
            }
        }
        Ok(Self {
            values,
            runtime_directory,
        })
    }

    /// The raw value and its source, when the operator set one.
    pub fn raw(&self, spec: SettingSpec) -> Option<(&OsStr, &Source)> {
        self.values
            .get(spec.name)
            .map(|(value, source)| (value.as_os_str(), source))
    }

    /// A raw value as text, or an `InvalidValue` naming the setting when
    /// the operating system gave bytes that are not UTF-8.
    fn text(&self, spec: SettingSpec) -> Result<Option<(&str, &Source)>, SettingsError> {
        match self.raw(spec) {
            None => Ok(None),
            Some((value, source)) => {
                value
                    .to_str()
                    .map(|text| Some((text, source)))
                    .ok_or_else(|| SettingsError::InvalidValue {
                        name: spec.name,
                        source: source.clone(),
                        value: value.to_string_lossy().into_owned(),
                        reason: "expected UTF-8 text".into(),
                    })
            }
        }
    }

    /// The controller socket: `WALDO_SOCKET_PATH`, or
    /// `$XDG_RUNTIME_DIR/waldo/controller.sock`.
    pub fn socket_path(&self) -> Result<PathBuf, SettingsError> {
        if let Some((value, _)) = self.raw(SOCKET_PATH) {
            return Ok(PathBuf::from(value));
        }
        self.runtime_directory
            .as_ref()
            .map(|runtime| runtime.join("waldo/controller.sock"))
            .ok_or(SettingsError::RuntimeDirectoryMissing)
    }

    /// Key names dispatched for clipboard paste. Names are validated against
    /// the active layout by the keyboard when the chord is executed.
    pub fn clipboard_paste_chord(&self) -> Result<Vec<String>, SettingsError> {
        let configured = self.text(CLIPBOARD_PASTE_CHORD)?;
        let value = configured.map_or_else(
            || {
                CLIPBOARD_PASTE_CHORD
                    .default
                    .expect("paste chord declares a default")
            },
            |(value, _)| value,
        );
        let keys: Vec<String> = value.split('+').map(|key| key.trim().to_owned()).collect();
        if keys.iter().any(String::is_empty) {
            let (_, source) = configured.expect("the default chord has no empty key");
            return Err(SettingsError::InvalidValue {
                name: CLIPBOARD_PASTE_CHORD.name,
                source: source.clone(),
                value: value.to_owned(),
                reason: "expected non-empty key names separated by '+'".into(),
            });
        }
        Ok(keys)
    }

    /// Maximum delivered MCP image edge: a positive whole number of pixels.
    pub fn model_image_max_edge(&self) -> Result<u32, SettingsError> {
        let (value, source) = match self.text(MODEL_IMAGE_MAX_EDGE)? {
            Some((value, source)) => (value, source),
            None => {
                return Ok(MODEL_IMAGE_MAX_EDGE
                    .default
                    .expect("image edge declares a default")
                    .parse()
                    .expect("the declared default parses"));
            }
        };
        value
            .parse::<u32>()
            .ok()
            .filter(|edge| *edge > 0)
            .ok_or_else(|| SettingsError::InvalidValue {
                name: MODEL_IMAGE_MAX_EDGE.name,
                source: source.clone(),
                value: value.to_owned(),
                reason: "expected a positive whole number of pixels fitting u32".into(),
            })
    }

    /// The driving lease's idle timeout: at least one second, never disabled.
    pub fn lease_idle_timeout(&self) -> Result<Duration, SettingsError> {
        let (value, source) = match self.text(LEASE_IDLE_TIMEOUT_SECONDS)? {
            Some((value, source)) => (value, source.clone()),
            None => {
                let default = LEASE_IDLE_TIMEOUT_SECONDS
                    .default
                    .expect("the lease idle timeout declares a default");
                return Ok(Duration::from_secs(
                    default.parse().expect("the declared default parses"),
                ));
            }
        };
        let seconds: u64 = value.parse().map_err(|_| SettingsError::InvalidValue {
            name: LEASE_IDLE_TIMEOUT_SECONDS.name,
            source: source.clone(),
            value: value.to_owned(),
            reason: "expected a whole number of seconds".into(),
        })?;
        if seconds == 0 {
            return Err(SettingsError::InvalidValue {
                name: LEASE_IDLE_TIMEOUT_SECONDS.name,
                source,
                value: value.to_owned(),
                reason: "the lease idle timeout cannot be disabled".into(),
            });
        }
        Ok(Duration::from_secs(seconds))
    }
}

/// `${XDG_CONFIG_HOME:-$HOME/.config}/waldo/settings`.
pub fn settings_file_path() -> PathBuf {
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    config_home.join("waldo/settings")
}

fn declared(name: &str) -> Option<SettingSpec> {
    ALL.iter().copied().find(|spec| spec.name == name)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        os::unix::ffi::OsStrExt,
        path::Path,
        time::Duration,
    };

    use super::{LEASE_IDLE_TIMEOUT_SECONDS, SOCKET_PATH, Settings, SettingsError, Source};

    fn settings(
        file: Option<&str>,
        environment: &[(&str, &str)],
    ) -> Result<Settings, SettingsError> {
        Settings::from_sources(
            file.map(|contents| (Path::new("/etc/waldo/settings"), contents)),
            environment
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
            Some("/run/user/1000".into()),
        )
    }

    #[test]
    fn paste_chord_default() {
        assert_eq!(
            settings(None, &[])
                .unwrap()
                .clipboard_paste_chord()
                .unwrap(),
            ["ctrl", "v"]
        );
    }

    #[test]
    fn paste_chord_resolution_and_whitespace() {
        let file = Some("WALDO_CLIPBOARD_PASTE_CHORD=shift+Insert\n");
        assert_eq!(
            settings(file, &[])
                .unwrap()
                .clipboard_paste_chord()
                .unwrap(),
            ["shift", "Insert"]
        );
        assert_eq!(
            settings(
                file,
                &[("WALDO_CLIPBOARD_PASTE_CHORD", " ctrl + shift + v ")]
            )
            .unwrap()
            .clipboard_paste_chord()
            .unwrap(),
            ["ctrl", "shift", "v"]
        );
        assert_eq!(
            settings(None, &[("WALDO_CLIPBOARD_PASTE_CHORD", "unknown-key")])
                .unwrap()
                .clipboard_paste_chord()
                .unwrap(),
            ["unknown-key"]
        );
    }

    #[test]
    fn paste_chord_rejects_empty_segments() {
        for value in ["", " ", "+v", "ctrl+", "ctrl++v", "ctrl+ +v"] {
            let error = settings(None, &[("WALDO_CLIPBOARD_PASTE_CHORD", value)])
                .unwrap()
                .clipboard_paste_chord()
                .unwrap_err();
            assert!(matches!(
                error,
                SettingsError::InvalidValue {
                    name: "WALDO_CLIPBOARD_PASTE_CHORD",
                    source: Source::Environment,
                    ..
                }
            ));
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let settings = settings(None, &[]).unwrap();
        assert_eq!(
            settings.socket_path().unwrap(),
            Path::new("/run/user/1000/waldo/controller.sock")
        );
        assert_eq!(
            settings.lease_idle_timeout().unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let settings = settings(
            Some("WALDO_LEASE_IDLE_TIMEOUT_SECONDS=90\nWALDO_SOCKET_PATH=/tmp/file.sock\n"),
            &[("WALDO_LEASE_IDLE_TIMEOUT_SECONDS", "45")],
        )
        .unwrap();
        assert_eq!(
            settings.lease_idle_timeout().unwrap(),
            Duration::from_secs(45)
        );
        assert_eq!(
            settings.raw(LEASE_IDLE_TIMEOUT_SECONDS),
            Some((OsStr::new("45"), &Source::Environment))
        );
        assert_eq!(settings.socket_path().unwrap(), Path::new("/tmp/file.sock"));
        assert!(matches!(
            settings.raw(SOCKET_PATH),
            Some((_, Source::File(path))) if path == Path::new("/etc/waldo/settings")
        ));
    }

    #[test]
    fn model_image_edge_defaults_to_2048() {
        assert_eq!(
            settings(None, &[]).unwrap().model_image_max_edge().unwrap(),
            2048
        );
    }

    #[test]
    fn model_image_edge_resolves_environment_over_file() {
        let file = "WALDO_MODEL_IMAGE_MAX_EDGE=640\n";
        assert_eq!(
            settings(Some(file), &[])
                .unwrap()
                .model_image_max_edge()
                .unwrap(),
            640
        );
        assert_eq!(
            settings(Some(file), &[("WALDO_MODEL_IMAGE_MAX_EDGE", "1280")])
                .unwrap()
                .model_image_max_edge()
                .unwrap(),
            1280
        );
    }

    #[test]
    fn model_image_edge_refuses_invalid_values_by_name() {
        for value in ["0", "no", "-1", "4294967296"] {
            let error = settings(None, &[("WALDO_MODEL_IMAGE_MAX_EDGE", value)])
                .unwrap()
                .model_image_max_edge()
                .unwrap_err();
            assert!(matches!(
                error,
                SettingsError::InvalidValue {
                    name: "WALDO_MODEL_IMAGE_MAX_EDGE",
                    ..
                }
            ));
        }
    }

    #[test]
    fn the_file_tolerates_comments_blank_lines_and_padding() {
        let settings = settings(
            Some("# lease\n\n  WALDO_LEASE_IDLE_TIMEOUT_SECONDS = 120 \n"),
            &[],
        )
        .unwrap();
        assert_eq!(
            settings.lease_idle_timeout().unwrap(),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn an_unknown_name_in_the_file_is_refused_with_its_line() {
        let error =
            settings(Some("WALDO_SOCKET_PATH=/a\nWALDO_LEASE_TIMEOUT=5\n"), &[]).unwrap_err();
        assert!(matches!(
            error,
            SettingsError::UnknownName { line: 2, ref name, .. } if name == "WALDO_LEASE_TIMEOUT"
        ));
    }

    #[test]
    fn a_line_without_an_equals_sign_is_malformed() {
        let error = settings(Some("WALDO_SOCKET_PATH\n"), &[]).unwrap_err();
        assert!(matches!(
            error,
            SettingsError::MalformedLine { line: 1, .. }
        ));
    }

    #[test]
    fn unrelated_environment_variables_are_ignored() {
        let settings = settings(None, &[("WALDO_LEASE_TIMEOUT", "5"), ("PATH", "/bin")]).unwrap();
        assert_eq!(
            settings.lease_idle_timeout().unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn the_lease_idle_timeout_cannot_be_disabled_or_malformed() {
        let disabled = settings(None, &[("WALDO_LEASE_IDLE_TIMEOUT_SECONDS", "0")])
            .unwrap()
            .lease_idle_timeout()
            .unwrap_err();
        assert!(matches!(
            disabled,
            SettingsError::InvalidValue { name: "WALDO_LEASE_IDLE_TIMEOUT_SECONDS", ref reason, .. }
                if reason.contains("cannot be disabled")
        ));
        let malformed = settings(None, &[("WALDO_LEASE_IDLE_TIMEOUT_SECONDS", "never")])
            .unwrap()
            .lease_idle_timeout()
            .unwrap_err();
        assert!(matches!(malformed, SettingsError::InvalidValue { .. }));
        assert_eq!(
            malformed.to_string(),
            "WALDO_LEASE_IDLE_TIMEOUT_SECONDS=\"never\" (from the process environment) is invalid: expected a whole number of seconds"
        );
    }

    #[test]
    fn a_socket_path_that_is_not_utf8_is_kept_byte_for_byte() {
        let raw = OsString::from(OsStr::from_bytes(b"/run/waldo-\xff/controller.sock"));
        let settings = Settings::from_sources(
            None,
            vec![(OsString::from("WALDO_SOCKET_PATH"), raw.clone())],
            None,
        )
        .unwrap();
        assert_eq!(settings.socket_path().unwrap().as_os_str(), raw.as_os_str());
        let timeout = Settings::from_sources(
            None,
            vec![(OsString::from("WALDO_LEASE_IDLE_TIMEOUT_SECONDS"), raw)],
            None,
        )
        .unwrap()
        .lease_idle_timeout()
        .unwrap_err();
        assert!(
            matches!(timeout, SettingsError::InvalidValue { ref reason, .. } if reason == "expected UTF-8 text")
        );
    }

    #[test]
    fn the_socket_needs_a_runtime_directory_when_not_set_explicitly() {
        let settings = Settings::from_sources(None, Vec::new(), None).unwrap();
        assert!(matches!(
            settings.socket_path(),
            Err(SettingsError::RuntimeDirectoryMissing)
        ));
    }
}
