use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use niri_ipc::{Action as NiriAction, Reply, Request, Response, WorkspaceReferenceArg};
use waldo_action::{ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{Action, ActionRoute, ErrorCode, ProtocolError, WindowOperation};

/// Executes the subset of Waldo actions which Niri can target by window ID.
#[derive(Clone, Copy, Debug, Default)]
pub struct NiriActionExecutor;

impl ActionExecutor for NiriActionExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionRoute, ActionExecutionFailure> {
        execute_with(action, cancellation, send_to_niri)
    }
}

fn execute_with(
    action: &Action,
    cancellation: &ExecutionToken,
    send: impl FnOnce(Request, &ExecutionToken) -> Result<Reply, ActionExecutionFailure>,
) -> Result<ActionRoute, ActionExecutionFailure> {
    if cancellation.is_cancelled() {
        return Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::Cancelled,
            message: "the action batch was cancelled before Niri dispatch".into(),
            retryable: false,
        }));
    }
    let request = translate(action).map_err(ActionExecutionFailure::not_executed)?;
    match send(request, cancellation)? {
        Ok(Response::Handled) => Ok(ActionRoute::NiriIpc),
        Ok(_) => Err(ActionExecutionFailure::indeterminate(
            ActionRoute::NiriIpc,
            backend_failure(
                "Niri returned an unexpected response to a window action",
                false,
            ),
        )),
        Err(message) => Err(ActionExecutionFailure::not_executed(backend_failure(
            format!("Niri rejected the window action: {message}"),
            false,
        ))),
    }
}

fn send_to_niri(
    request: Request,
    cancellation: &ExecutionToken,
) -> Result<Reply, ActionExecutionFailure> {
    let path = env::var_os(niri_ipc::socket::SOCKET_PATH_ENV).ok_or_else(|| {
        ActionExecutionFailure::not_executed(backend_failure(
            format!(
                "{} is not set; Niri IPC is unavailable",
                niri_ipc::socket::SOCKET_PATH_ENV
            ),
            true,
        ))
    })?;
    let mut stream = UnixStream::connect(path).map_err(|error| {
        ActionExecutionFailure::not_executed(transport_failure("connect to Niri", error))
    })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| {
            ActionExecutionFailure::not_executed(transport_failure(
                "configure Niri IPC writes",
                error,
            ))
        })?;
    let mut message = serde_json::to_vec(&request).map_err(|error| {
        ActionExecutionFailure::not_executed(backend_failure(
            format!("could not encode Niri action: {error}"),
            false,
        ))
    })?;
    message.push(b'\n');
    if cancellation.is_cancelled() {
        return Err(ActionExecutionFailure::not_executed(cancelled()));
    }
    stream.write_all(&message).map_err(|error| {
        ActionExecutionFailure::not_executed(transport_failure("send the Niri action", error))
    })?;
    // The newline makes this request actionable. Cancellation cannot retract it,
    // so await a bounded authoritative reply and keep any lost reply honest.
    read_dispatched_reply(stream, Duration::from_secs(2))
}

fn read_dispatched_reply(
    mut stream: UnixStream,
    timeout: Duration,
) -> Result<Reply, ActionExecutionFailure> {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    const MAX_REPLY_BYTES: usize = 1024 * 1024;

    stream
        .set_read_timeout(Some(POLL_INTERVAL))
        .map_err(|error| indeterminate_transport_failure("configure Niri IPC reads", error))?;
    let deadline = Instant::now() + timeout;
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(ActionExecutionFailure::indeterminate(
                ActionRoute::NiriIpc,
                backend_failure(
                    "Niri did not acknowledge the dispatched action before the reply deadline",
                    true,
                ),
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(ActionExecutionFailure::indeterminate(
                    ActionRoute::NiriIpc,
                    backend_failure(
                        "Niri IPC closed after dispatch without returning an action response",
                        true,
                    ),
                ));
            }
            Ok(read) => {
                reply.extend_from_slice(&chunk[..read]);
                if reply.len() > MAX_REPLY_BYTES {
                    return Err(ActionExecutionFailure::indeterminate(
                        ActionRoute::NiriIpc,
                        backend_failure(
                            "Niri IPC returned an oversized action response after dispatch",
                            false,
                        ),
                    ));
                }
                if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
                    return serde_json::from_slice(&reply[..end]).map_err(|error| {
                        ActionExecutionFailure::indeterminate(
                            ActionRoute::NiriIpc,
                            backend_failure(
                                format!("Niri returned an invalid action response: {error}"),
                                false,
                            ),
                        )
                    });
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                return Err(indeterminate_transport_failure(
                    "read the Niri action response",
                    error,
                ));
            }
        }
    }
}

fn translate(action: &Action) -> Result<Request, ProtocolError> {
    let Action::Window {
        window_id,
        operation,
    } = action
    else {
        return Err(unsupported("this action has no Niri IPC route"));
    };
    let id = window_id.0.parse::<u64>().map_err(|_| ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: "window IDs supplied to Niri must be unsigned integers".into(),
        retryable: false,
    })?;

    let action = match operation {
        WindowOperation::Focus => NiriAction::FocusWindow { id },
        WindowOperation::Close => NiriAction::CloseWindow { id: Some(id) },
        WindowOperation::MoveToWorkspace { workspace } => NiriAction::MoveWindowToWorkspace {
            window_id: Some(id),
            reference: WorkspaceReferenceArg::Name(workspace.clone()),
            focus: false,
        },
        WindowOperation::MoveToOutput { output } => NiriAction::MoveWindowToMonitor {
            id: Some(id),
            output: output.clone(),
        },
        WindowOperation::SetSize { .. } => {
            return Err(unsupported("atomic Niri window resizing is not available"));
        }
    };
    Ok(Request::Action(action))
}

fn unsupported(message: &str) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: message.into(),
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

fn transport_failure(operation: &str, error: io::Error) -> ProtocolError {
    backend_failure(
        format!("Niri IPC failed while trying to {operation}: {error}"),
        true,
    )
}

fn indeterminate_transport_failure(operation: &str, error: io::Error) -> ActionExecutionFailure {
    ActionExecutionFailure::indeterminate(ActionRoute::NiriIpc, transport_failure(operation, error))
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled before Niri dispatch".into(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action as NiriAction, Request, Response, WorkspaceReferenceArg};
    use waldo_action::ActionExecutionFailure;
    use waldo_protocol::{Action, ActionRoute, ClientId, ErrorCode, WindowId, WindowOperation};

    use super::{execute_with, translate};
    use waldo_controller::{Controller, ExecutionToken};

    fn window(operation: WindowOperation) -> Action {
        Action::Window {
            window_id: WindowId("42".into()),
            operation,
        }
    }

    #[test]
    fn translates_targeted_focus_close_and_moves() {
        assert!(matches!(
            translate(&window(WindowOperation::Focus)),
            Ok(Request::Action(NiriAction::FocusWindow { id: 42 }))
        ));
        assert!(matches!(
            translate(&window(WindowOperation::Close)),
            Ok(Request::Action(NiriAction::CloseWindow { id: Some(42) }))
        ));
        assert!(matches!(
            translate(&window(WindowOperation::MoveToWorkspace {
                workspace: "research".into(),
            })),
            Ok(Request::Action(NiriAction::MoveWindowToWorkspace {
                window_id: Some(42),
                reference: WorkspaceReferenceArg::Name(name),
                focus: false,
            })) if name == "research"
        ));
        assert!(matches!(
            translate(&window(WindowOperation::MoveToOutput {
                output: "DP-2".into(),
            })),
            Ok(Request::Action(NiriAction::MoveWindowToMonitor {
                id: Some(42),
                output,
            })) if output == "DP-2"
        ));
    }

    #[test]
    fn rejects_invalid_ids_resize_and_non_niri_actions() {
        let invalid = Action::Window {
            window_id: WindowId("not-niri".into()),
            operation: WindowOperation::Focus,
        };
        assert_eq!(
            translate(&invalid).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            translate(&window(WindowOperation::SetSize {
                width: 800,
                height: 600,
            }))
            .unwrap_err()
            .code,
            ErrorCode::UnsupportedCapability
        );
        assert_eq!(
            translate(&Action::KeyChord {
                keys: vec!["Ctrl".into(), "L".into()],
            })
            .unwrap_err()
            .code,
            ErrorCode::UnsupportedCapability
        );
    }

    #[test]
    fn executor_reports_the_niri_route_after_successful_dispatch() {
        let result = execute_with(
            &window(WindowOperation::Focus),
            &test_token(),
            |request, _| {
                assert!(matches!(
                    request,
                    Request::Action(NiriAction::FocusWindow { id: 42 })
                ));
                Ok(Ok(Response::Handled))
            },
        );
        assert_eq!(result, Ok(ActionRoute::NiriIpc));
    }

    #[test]
    fn executor_maps_rejection_transport_failure_and_cancellation() {
        let rejected = execute_with(&window(WindowOperation::Focus), &test_token(), |_, _| {
            Ok(Err("window vanished".into()))
        })
        .unwrap_err();
        assert!(matches!(
            rejected,
            ActionExecutionFailure::NotExecuted { error }
                if error.code == ErrorCode::BackendFailure && !error.retryable
        ));

        let transport = execute_with(&window(WindowOperation::Focus), &test_token(), |_, _| {
            Err(super::indeterminate_transport_failure(
                "read test response",
                std::io::Error::new(std::io::ErrorKind::ConnectionReset, "socket closed"),
            ))
        })
        .unwrap_err();
        assert!(matches!(
            transport,
            ActionExecutionFailure::Indeterminate {
                route: ActionRoute::NiriIpc,
                error,
            } if error.code == ErrorCode::BackendFailure && error.retryable
        ));

        let token = test_token();
        token.cancel();
        let cancelled = execute_with(&window(WindowOperation::Focus), &token, |_, _| {
            panic!("a cancelled action must not reach Niri")
        })
        .unwrap_err();
        assert!(matches!(
            cancelled,
            ActionExecutionFailure::NotExecuted { error }
                if error.code == ErrorCode::Cancelled
        ));
    }

    #[test]
    fn a_stalled_post_dispatch_reply_becomes_indeterminate() {
        let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();

        let failure =
            super::read_dispatched_reply(client, std::time::Duration::from_millis(75)).unwrap_err();
        assert!(matches!(
            failure,
            ActionExecutionFailure::Indeterminate {
                route: ActionRoute::NiriIpc,
                error,
            } if error.code == ErrorCode::BackendFailure
        ));
    }

    #[test]
    fn live_niri_transport_round_trips_a_read_only_request() {
        if std::env::var_os(niri_ipc::socket::SOCKET_PATH_ENV).is_none() {
            return;
        }

        assert!(matches!(
            super::send_to_niri(Request::Version, &test_token()),
            Ok(Ok(Response::Version(_)))
        ));
    }

    fn test_token() -> ExecutionToken {
        let mut controller = Controller::new(std::time::Duration::from_secs(30));
        let connection = controller.connect(ClientId::new("niri-action-test").unwrap());
        controller
            .begin_execution(connection, std::time::Duration::ZERO)
            .unwrap()
            .cancellation()
            .clone()
    }
}
