//! MIME-typed clipboard execution through Wayland data-control.

use std::io::Read;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{Action, ActionOutput, ActionRoute, ErrorCode, ProtocolError};
use wl_clipboard_rs::{
    copy::{
        ClipboardType as CopyClipboardType, MimeSource, MimeType as CopyMimeType, Options,
        ServeRequests, Source, clear,
    },
    paste::{
        ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat, get_contents,
        get_mime_types,
    },
};

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Reads and owns the regular Wayland clipboard through data-control.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClipboardExecutor;

/// Complete bounded clipboard contents retained while a temporary paste is served.
pub struct ClipboardSnapshot {
    sources: Vec<(String, Vec<u8>)>,
}

impl ClipboardExecutor {
    /// Probes the seat and data-control protocol before advertising support.
    pub fn initialise() -> Result<Self, ProtocolError> {
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(_) | Err(PasteError::ClipboardEmpty) => Ok(Self),
            Err(error) => Err(backend_failure(
                format!("initialise Wayland clipboard: {error}"),
                false,
            )),
        }
    }

    /// Reads every advertised MIME representation for later restoration.
    pub fn snapshot(&self) -> Result<ClipboardSnapshot, ProtocolError> {
        let mime_types = match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(mime_types) => mime_types,
            Err(PasteError::ClipboardEmpty) => {
                return Ok(ClipboardSnapshot {
                    sources: Vec::new(),
                });
            }
            Err(error) => {
                return Err(backend_failure(
                    format!("snapshot Wayland clipboard MIME types: {error}"),
                    true,
                ));
            }
        };
        let mut sources = Vec::with_capacity(mime_types.len());
        let mut total = 0_usize;
        for mime_type in mime_types {
            validate_mime_type(&mime_type)?;
            let (reader, _) = get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                PasteMimeType::Specific(&mime_type),
            )
            .map_err(|error| {
                backend_failure(
                    format!("snapshot Wayland clipboard as {mime_type}: {error}"),
                    true,
                )
            })?;
            let remaining = MAX_CLIPBOARD_BYTES.saturating_sub(total);
            let mut data = Vec::new();
            reader
                .take((remaining + 1) as u64)
                .read_to_end(&mut data)
                .map_err(|error| {
                    backend_failure(format!("receive clipboard snapshot data: {error}"), true)
                })?;
            total = total.saturating_add(data.len());
            if total > MAX_CLIPBOARD_BYTES {
                return Err(ProtocolError {
                    code: ErrorCode::UnsupportedCapability,
                    message: format!(
                        "clipboard representations exceed Waldo's {MAX_CLIPBOARD_BYTES}-byte restoration limit"
                    ),
                    retryable: false,
                });
            }
            sources.push((mime_type, data));
        }
        Ok(ClipboardSnapshot { sources })
    }

    /// Offers temporary UTF-8 text and retires it after one paste request.
    pub fn offer_text_once(&self, text: &str) -> Result<(), ActionExecutionFailure> {
        let mut options = Options::new();
        options.serve_requests(ServeRequests::Only(1));
        options
            .copy(Source::Bytes(text.as_bytes().into()), CopyMimeType::Text)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("offer temporary paste text: {error}"), true),
                )
            })
    }

    /// Reports whether the one-shot temporary clipboard has been consumed.
    pub fn is_empty(&self) -> Result<bool, ProtocolError> {
        match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(_) => Ok(false),
            Err(PasteError::ClipboardEmpty) => Ok(true),
            Err(error) => Err(backend_failure(
                format!("inspect temporary Wayland clipboard: {error}"),
                true,
            )),
        }
    }

    /// Restores every MIME representation captured before a temporary paste.
    pub fn restore(&self, snapshot: ClipboardSnapshot) -> Result<(), ProtocolError> {
        if snapshot.sources.is_empty() {
            return clear(CopyClipboardType::Regular, wl_clipboard_rs::copy::Seat::All).map_err(
                |error| backend_failure(format!("restore empty Wayland clipboard: {error}"), true),
            );
        }
        let sources = snapshot
            .sources
            .into_iter()
            .map(|(mime_type, data)| MimeSource {
                source: Source::Bytes(data.into()),
                mime_type: CopyMimeType::Specific(mime_type),
            })
            .collect();
        let mut options = Options::new();
        options.omit_additional_text_mime_types(true);
        options.copy_multi(sources).map_err(|error| {
            backend_failure(format!("restore Wayland clipboard contents: {error}"), true)
        })
    }

    fn read(mime_type: &str) -> Result<ActionExecution, ActionExecutionFailure> {
        validate_mime_type(mime_type).map_err(ActionExecutionFailure::not_executed)?;
        let (reader, selected_mime) = get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            PasteMimeType::Specific(mime_type),
        )
        .map_err(|error| {
            ActionExecutionFailure::not_executed(backend_failure(
                format!("read Wayland clipboard as {mime_type}: {error}"),
                true,
            ))
        })?;
        let mut data = Vec::new();
        reader
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("receive Wayland clipboard data: {error}"), true),
                )
            })?;
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "clipboard value exceeds Waldo's {MAX_CLIPBOARD_BYTES}-byte transfer limit"
                ),
                retryable: false,
            }));
        }

        Ok(ActionExecution {
            route: ActionRoute::Clipboard,
            output: Some(ActionOutput::Clipboard {
                mime_type: selected_mime,
                data_base64: BASE64.encode(data),
            }),
        })
    }

    fn write(
        mime_type: &str,
        data_base64: &str,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        validate_mime_type(mime_type).map_err(ActionExecutionFailure::not_executed)?;
        let data = BASE64.decode(data_base64).map_err(|error| {
            ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: format!("clipboard data is not valid base64: {error}"),
                retryable: false,
            })
        })?;
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: format!(
                    "clipboard value exceeds Waldo's {MAX_CLIPBOARD_BYTES}-byte transfer limit"
                ),
                retryable: false,
            }));
        }
        Options::new()
            .copy(
                Source::Bytes(data.into()),
                CopyMimeType::Specific(mime_type.to_owned()),
            )
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("write Wayland clipboard: {error}"), true),
                )
            })?;
        Ok(ActionRoute::Clipboard.into())
    }
}

impl ActionExecutor for ClipboardExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        match action {
            Action::ClipboardRead { mime_type } => Self::read(mime_type),
            Action::ClipboardWrite {
                mime_type,
                data_base64,
            } => Self::write(mime_type, data_base64),
            _ => Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "this action has no clipboard route".into(),
                retryable: false,
            })),
        }
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::Clipboard
    }
}

fn validate_mime_type(mime_type: &str) -> Result<(), ProtocolError> {
    if mime_type.is_empty() || mime_type.contains('\0') {
        return Err(ProtocolError {
            code: ErrorCode::InvalidRequest,
            message: "clipboard MIME type must be non-empty and contain no NUL byte".into(),
            retryable: false,
        });
    }
    Ok(())
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled before clipboard dispatch".into(),
        retryable: false,
    }
}

fn backend_failure(message: impl Into<String>, retryable: bool) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message: message.into(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use waldo_protocol::ErrorCode;

    use super::validate_mime_type;

    #[test]
    fn mime_types_fail_closed_before_wayland_dispatch() {
        assert_eq!(
            validate_mime_type("").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            validate_mime_type("text/plain\0hidden").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert!(validate_mime_type("text/plain;charset=utf-8").is_ok());
    }
}
