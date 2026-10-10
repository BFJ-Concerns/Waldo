use std::{
    env,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use niri_ipc::{Action as NiriAction, Reply, Request, Response, SizeChange, WorkspaceReferenceArg};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{Action, ActionRoute, ProtocolError, WindowOperation, WorkspaceOperation};

/// Executes the Waldo actions Niri can target positively: window actions by
/// window ID and workspace actions by workspace ID.
#[derive(Clone, Copy, Debug, Default)]
pub struct NiriActionExecutor;

impl ActionExecutor for NiriActionExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        execute_with(action, cancellation, send_to_niri)
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::NiriIpc
    }
}

fn execute_with(
    action: &Action,
    cancellation: &ExecutionToken,
    mut send: impl FnMut(Request, &ExecutionToken) -> Result<Reply, ActionExecutionFailure>,
) -> Result<ActionExecution, ActionExecutionFailure> {
    if cancellation.is_cancelled() {
        return Err(ActionExecutionFailure::not_executed(cancelled()));
    }
    let requests = translate(action).map_err(ActionExecutionFailure::not_executed)?;
    for (index, request) in requests.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            let failure = ActionExecutionFailure::not_executed(cancelled());
            return Err(if index == 0 {
                failure
            } else {
                partial_execution(failure)
            });
        }
        let reply = send(request, cancellation).map_err(|failure| {
            if index == 0 {
                failure
            } else {
                partial_execution(failure)
            }
        })?;
        match reply {
            Ok(Response::Handled) => {}
            Ok(_) => {
                return Err(ActionExecutionFailure::indeterminate(
                    ActionRoute::NiriIpc,
                    ProtocolError::backend_failure(
                        "Niri returned an unexpected response after action dispatch",
                        false,
                    ),
                ));
            }
            Err(message) if index == 0 => {
                return Err(ActionExecutionFailure::not_executed(
                    ProtocolError::backend_failure(
                        format!("Niri rejected the action: {message}"),
                        false,
                    ),
                ));
            }
            Err(message) => {
                return Err(ActionExecutionFailure::indeterminate(
                    ActionRoute::NiriIpc,
                    ProtocolError::backend_failure(
                        format!(
                            "Niri rejected a later resize step after changing the window: {message}"
                        ),
                        false,
                    ),
                ));
            }
        }
    }
    Ok(ActionRoute::NiriIpc.into())
}

fn partial_execution(failure: ActionExecutionFailure) -> ActionExecutionFailure {
    let error = match failure {
        ActionExecutionFailure::NotExecuted { error }
        | ActionExecutionFailure::Indeterminate { error, .. } => error,
    };
    ActionExecutionFailure::indeterminate(
        ActionRoute::NiriIpc,
        ProtocolError {
            message: format!(
                "Niri changed part of the requested window size: {}",
                error.message
            ),
            ..error
        },
    )
}

fn send_to_niri(
    request: Request,
    cancellation: &ExecutionToken,
) -> Result<Reply, ActionExecutionFailure> {
    let path = env::var_os(niri_ipc::socket::SOCKET_PATH_ENV).ok_or_else(|| {
        ActionExecutionFailure::not_executed(ProtocolError::backend_failure(
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
        ActionExecutionFailure::not_executed(ProtocolError::backend_failure(
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
                ProtocolError::backend_failure(
                    "Niri did not acknowledge the dispatched action before the reply deadline",
                    true,
                ),
            ));
        }
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(ActionExecutionFailure::indeterminate(
                    ActionRoute::NiriIpc,
                    ProtocolError::backend_failure(
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
                        ProtocolError::backend_failure(
                            "Niri IPC returned an oversized action response after dispatch",
                            false,
                        ),
                    ));
                }
                if let Some(end) = reply.iter().position(|byte| *byte == b'\n') {
                    return serde_json::from_slice(&reply[..end]).map_err(|error| {
                        ActionExecutionFailure::indeterminate(
                            ActionRoute::NiriIpc,
                            ProtocolError::backend_failure(
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

fn translate(action: &Action) -> Result<Vec<Request>, ProtocolError> {
    let actions = match action {
        Action::Window {
            window_id,
            operation,
        } => {
            let id = window_id.0.parse::<u64>().map_err(|_| {
                ProtocolError::invalid_request(
                    "window IDs supplied to Niri must be unsigned integers",
                )
            })?;
            match operation {
                WindowOperation::Focus => vec![NiriAction::FocusWindow { id }],
                WindowOperation::Close => vec![NiriAction::CloseWindow { id: Some(id) }],
                WindowOperation::MoveToWorkspace { workspace_id } => {
                    vec![NiriAction::MoveWindowToWorkspace {
                        window_id: Some(id),
                        reference: WorkspaceReferenceArg::Id(workspace_id.get()),
                        focus: false,
                    }]
                }
                WindowOperation::MoveToOutput { output } => {
                    vec![NiriAction::MoveWindowToMonitor {
                        id: Some(id),
                        output: output.clone(),
                    }]
                }
                WindowOperation::SetSize { width, height } => {
                    let width = fixed_size(*width)?;
                    let height = fixed_size(*height)?;
                    vec![
                        NiriAction::SetWindowWidth {
                            id: Some(id),
                            change: SizeChange::SetFixed(width),
                        },
                        NiriAction::SetWindowHeight {
                            id: Some(id),
                            change: SizeChange::SetFixed(height),
                        },
                    ]
                }
            }
        }
        Action::Workspace {
            workspace_id,
            operation,
        } => match operation {
            WorkspaceOperation::Focus => vec![NiriAction::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(workspace_id.get()),
            }],
            WorkspaceOperation::MoveToOutput { output } => {
                vec![NiriAction::MoveWorkspaceToMonitor {
                    output: output.clone(),
                    reference: Some(WorkspaceReferenceArg::Id(workspace_id.get())),
                }]
            }
        },
        _ => {
            return Err(ProtocolError::unsupported_capability(
                "this action has no Niri IPC route",
            ));
        }
    };
    Ok(actions.into_iter().map(Request::Action).collect())
}

fn fixed_size(value: u32) -> Result<i32, ProtocolError> {
    if value == 0 {
        return Err(ProtocolError::invalid_request(
            "window dimensions must be greater than zero",
        ));
    }
    i32::try_from(value).map_err(|_| {
        ProtocolError::invalid_request("window dimension exceeds Niri's supported range")
    })
}

fn transport_failure(operation: &str, error: io::Error) -> ProtocolError {
    ProtocolError::backend_failure(
        format!("Niri IPC failed while trying to {operation}: {error}"),
        true,
    )
}

fn indeterminate_transport_failure(operation: &str, error: io::Error) -> ActionExecutionFailure {
    ActionExecutionFailure::indeterminate(ActionRoute::NiriIpc, transport_failure(operation, error))
}

fn cancelled() -> ProtocolError {
    ProtocolError::cancelled("the action batch was cancelled before Niri dispatch")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action as NiriAction, Request, Response, SizeChange, WorkspaceReferenceArg};
    use waldo_action::ActionExecutionFailure;
    use waldo_protocol::{
        Action, ActionRoute, ErrorCode, WindowId, WindowOperation, WorkspaceId, WorkspaceOperation,
    };

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
            Ok(requests) if matches!(
                requests.as_slice(),
                [Request::Action(NiriAction::FocusWindow { id: 42 })]
            )
        ));
        assert!(matches!(
            translate(&window(WindowOperation::Close)),
            Ok(requests) if matches!(
                requests.as_slice(),
                [Request::Action(NiriAction::CloseWindow { id: Some(42) })]
            )
        ));
        assert!(matches!(
            translate(&window(WindowOperation::MoveToWorkspace {
                workspace_id: WorkspaceId::new(7),
            })),
            Ok(requests) if matches!(
                requests.as_slice(),
                [Request::Action(NiriAction::MoveWindowToWorkspace {
                    window_id: Some(42),
                    reference: WorkspaceReferenceArg::Id(7),
                    focus: false,
                })]
            )
        ));
        assert!(matches!(
            translate(&window(WindowOperation::MoveToOutput {
                output: "DP-2".into(),
            })),
            Ok(requests) if matches!(
                requests.as_slice(),
                [Request::Action(NiriAction::MoveWindowToMonitor {
                    id: Some(42),
                    output,
                })] if output == "DP-2"
            )
        ));
    }

    #[test]
    fn translates_resize_and_workspace_actions() {
        assert!(matches!(
            translate(&window(WindowOperation::SetSize {
                width: 800,
                height: 600,
            })),
            Ok(requests) if matches!(
                requests.as_slice(),
                [
                    Request::Action(NiriAction::SetWindowWidth {
                        id: Some(42),
                        change: SizeChange::SetFixed(800),
                    }),
                    Request::Action(NiriAction::SetWindowHeight {
                        id: Some(42),
                        change: SizeChange::SetFixed(600),
                    }),
                ]
            )
        ));
        assert!(matches!(
            translate(&Action::Workspace {
                workspace_id: WorkspaceId::new(7),
                operation: WorkspaceOperation::Focus,
            }),
            Ok(requests) if matches!(
                requests.as_slice(),
                [Request::Action(NiriAction::FocusWorkspace {
                    reference: WorkspaceReferenceArg::Id(7),
                })]
            )
        ));
    }

    #[test]
    fn rejects_invalid_ids_sizes_and_non_niri_actions() {
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
                width: 0,
                height: 600,
            }))
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
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
        assert_eq!(result, Ok(ActionRoute::NiriIpc.into()));
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
    fn resize_reports_indeterminate_when_only_one_dimension_changed() {
        let mut dispatched = 0;
        let failure = execute_with(
            &window(WindowOperation::SetSize {
                width: 800,
                height: 600,
            }),
            &test_token(),
            |_, _| {
                dispatched += 1;
                if dispatched == 1 {
                    Ok(Ok(Response::Handled))
                } else {
                    Ok(Err("height rejected".into()))
                }
            },
        )
        .unwrap_err();

        assert_eq!(dispatched, 2);
        assert!(matches!(
            failure,
            ActionExecutionFailure::Indeterminate {
                route: ActionRoute::NiriIpc,
                error,
            } if error.message.contains("later resize step")
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
        let connection = controller.connect();
        controller
            .begin_execution(connection, std::time::Duration::ZERO)
            .unwrap()
            .cancellation()
            .clone()
    }
}
