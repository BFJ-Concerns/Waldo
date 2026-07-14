//! Wayland virtual-pointer execution with held-button containment.

use std::{
    collections::BTreeSet,
    sync::Mutex,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_niri::NiriInspector;
use waldo_protocol::{
    Action, ActionRoute, ErrorCode, InputState, LogicalPoint, PointerButton, ProtocolError,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_pointer::{Axis, ButtonState},
        wl_registry,
    },
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
const PATH_STEP: Duration = Duration::from_millis(8);

/// Executes Waldo pointer actions through the compositor's virtual-pointer protocol.
pub struct VirtualPointerExecutor {
    state: Mutex<Option<PointerState>>,
}

impl VirtualPointerExecutor {
    /// Initialises the Wayland device so capability reporting reflects reality.
    pub fn initialise() -> Result<Self, ProtocolError> {
        Ok(Self {
            state: Mutex::new(Some(PointerState::connect()?)),
        })
    }

    fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut PointerState) -> Result<T, ActionExecutionFailure>,
    ) -> Result<T, ActionExecutionFailure> {
        let mut guard = self.state.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(backend_failure(
                "virtual-pointer state lock is poisoned",
                false,
            ))
        })?;
        if guard.is_none() {
            *guard = Some(PointerState::connect().map_err(ActionExecutionFailure::not_executed)?);
        }
        let result = operation(guard.as_mut().expect("pointer state was initialised"));
        if matches!(result, Err(ActionExecutionFailure::Indeterminate { .. })) {
            // Closing the device is the only honest recovery after the socket
            // can no longer tell us which queued requests reached Niri.
            guard.take();
        }
        result
    }
}

impl ActionExecutor for VirtualPointerExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        self.with_state(|state| state.execute(action, cancellation))?;
        Ok(ActionRoute::VirtualPointer.into())
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::VirtualPointer
    }

    fn release_all(&self) {
        if let Ok(mut state) = self.state.lock() {
            // Destroying the virtual device is the compositor-owned release-all
            // boundary, including after an interrupted explicit press.
            state.take();
        }
    }
}

struct PointerState {
    connection: Connection,
    queue: EventQueue<WaylandState>,
    pointer: ZwlrVirtualPointerV1,
    held_buttons: BTreeSet<u32>,
}

impl PointerState {
    fn connect() -> Result<Self, ProtocolError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
        let (globals, queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|error| backend_failure(format!("read Wayland globals: {error}"), true))?;
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 1..=2, ()).map_err(|error| {
                backend_failure(format!("bind virtual-pointer manager: {error}"), false)
            })?;
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        connection
            .flush()
            .map_err(|error| backend_failure(format!("create virtual pointer: {error}"), true))?;
        Ok(Self {
            connection,
            queue,
            pointer,
            held_buttons: BTreeSet::new(),
        })
    }

    fn execute(
        &mut self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<(), ActionExecutionFailure> {
        let prepared =
            PreparedAction::from_action(action).map_err(ActionExecutionFailure::not_executed)?;
        cancelled_before_dispatch(cancellation)?;

        match prepared {
            PreparedAction::MoveAbsolute {
                x,
                y,
                width,
                height,
            } => {
                self.pointer
                    .motion_absolute(timestamp(), x, y, width, height);
                self.pointer.frame();
            }
            PreparedAction::MoveRelative { x, y } => {
                self.pointer.motion(timestamp(), x, y);
                self.pointer.frame();
            }
            PreparedAction::Button { button, state } => self.button(button, state),
            PreparedAction::Click {
                x,
                y,
                width,
                height,
                button,
                count,
            } => {
                self.pointer
                    .motion_absolute(timestamp(), x, y, width, height);
                self.pointer.frame();
                for _ in 0..count {
                    self.button(button, InputState::Pressed);
                    self.button(button, InputState::Released);
                }
            }
            PreparedAction::Drag {
                points,
                button,
                delay,
            } => {
                let first = points.first().expect("prepared drag is non-empty");
                self.send_absolute(*first);
                self.button(button, InputState::Pressed);
                self.flush_dispatched("start virtual-pointer drag")?;
                let result = self.follow_prepared_path(&points[1..], delay, cancellation, true);
                self.button(button, InputState::Released);
                self.flush_dispatched("release virtual-pointer drag button")?;
                result?;
            }
            PreparedAction::Path { points, delay } => {
                self.follow_prepared_path(&points, delay, cancellation, false)?;
                return Ok(());
            }
            PreparedAction::Scroll { x, y } => {
                let time = timestamp();
                if x != 0.0 {
                    self.pointer.axis(time, Axis::HorizontalScroll, x);
                }
                if y != 0.0 {
                    self.pointer.axis(time, Axis::VerticalScroll, y);
                }
                self.pointer.frame();
            }
        }
        self.flush_dispatched("flush virtual-pointer action")?;
        self.queue
            .dispatch_pending(&mut WaylandState)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::VirtualPointer,
                    backend_failure(
                        format!("dispatch virtual-pointer protocol state: {error}"),
                        true,
                    ),
                )
            })?;
        Ok(())
    }

    fn send_absolute(&self, point: PreparedPoint) {
        self.pointer
            .motion_absolute(timestamp(), point.x, point.y, point.width, point.height);
        self.pointer.frame();
    }

    fn button(&mut self, button: PointerButton, state: InputState) {
        let code = button_code(button);
        let wayland_state = match state {
            InputState::Pressed => {
                self.held_buttons.insert(code);
                ButtonState::Pressed
            }
            InputState::Released => {
                self.held_buttons.remove(&code);
                ButtonState::Released
            }
        };
        self.pointer.button(timestamp(), code, wayland_state);
        self.pointer.frame();
    }

    fn follow_prepared_path(
        &mut self,
        points: &[PreparedPoint],
        delay: Duration,
        cancellation: &ExecutionToken,
        already_dispatched: bool,
    ) -> Result<(), ActionExecutionFailure> {
        let mut dispatched = already_dispatched;
        for (index, point) in points.iter().enumerate() {
            cancellation_boundary(cancellation, dispatched)?;
            if index > 0 || already_dispatched {
                sleep_cancellable(delay, cancellation, dispatched)?;
            }
            self.send_absolute(*point);
            self.flush_dispatched("flush virtual-pointer path")?;
            dispatched = true;
        }
        Ok(())
    }

    fn flush_dispatched(&self, context: &str) -> Result<(), ActionExecutionFailure> {
        self.connection.flush().map_err(|error| {
            ActionExecutionFailure::indeterminate(
                ActionRoute::VirtualPointer,
                backend_failure(format!("{context}: {error}"), true),
            )
        })
    }
}

enum PreparedAction {
    MoveAbsolute {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    MoveRelative {
        x: f64,
        y: f64,
    },
    Button {
        button: PointerButton,
        state: InputState,
    },
    Click {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        button: PointerButton,
        count: u8,
    },
    Drag {
        points: Vec<PreparedPoint>,
        button: PointerButton,
        delay: Duration,
    },
    Path {
        points: Vec<PreparedPoint>,
        delay: Duration,
    },
    Scroll {
        x: f64,
        y: f64,
    },
}

impl PreparedAction {
    fn from_action(action: &Action) -> Result<Self, ProtocolError> {
        match action {
            Action::PointerMoveAbsolute { position } => {
                let point = PreparedPoint::new(*position, &DesktopGeometry::inspect()?)?;
                Ok(Self::MoveAbsolute {
                    x: point.x,
                    y: point.y,
                    width: point.width,
                    height: point.height,
                })
            }
            Action::PointerMoveRelative { delta } => {
                finite(delta.x, "relative pointer x")?;
                finite(delta.y, "relative pointer y")?;
                Ok(Self::MoveRelative {
                    x: delta.x,
                    y: delta.y,
                })
            }
            Action::PointerButton { button, state } => Ok(Self::Button {
                button: *button,
                state: *state,
            }),
            Action::PointerClick {
                position,
                button,
                count,
            } => {
                if *count == 0 {
                    return Err(invalid("pointer click count must be greater than zero"));
                }
                let point = PreparedPoint::new(*position, &DesktopGeometry::inspect()?)?;
                Ok(Self::Click {
                    x: point.x,
                    y: point.y,
                    width: point.width,
                    height: point.height,
                    button: *button,
                    count: *count,
                })
            }
            Action::PointerDrag {
                from,
                to,
                button,
                duration_ms,
            } => {
                let geometry = DesktopGeometry::inspect()?;
                let steps = (u64::from(*duration_ms).div_ceil(PATH_STEP.as_millis() as u64)).max(1)
                    as usize;
                let mut points = Vec::with_capacity(steps + 1);
                for step in 0..=steps {
                    let progress = step as f64 / steps as f64;
                    points.push(PreparedPoint::new(
                        LogicalPoint {
                            x: from.x + (to.x - from.x) * progress,
                            y: from.y + (to.y - from.y) * progress,
                        },
                        &geometry,
                    )?);
                }
                Ok(Self::Drag {
                    points,
                    button: *button,
                    delay: Duration::from_millis(u64::from(*duration_ms)) / steps as u32,
                })
            }
            Action::PointerPath {
                points,
                duration_ms,
            } => {
                if points.is_empty() {
                    return Err(invalid("pointer path must contain at least one point"));
                }
                let geometry = DesktopGeometry::inspect()?;
                let points = points
                    .iter()
                    .map(|point| PreparedPoint::new(*point, &geometry))
                    .collect::<Result<Vec<_>, _>>()?;
                let segments = points.len().saturating_sub(1).max(1) as u32;
                Ok(Self::Path {
                    points,
                    delay: Duration::from_millis(u64::from(*duration_ms)) / segments,
                })
            }
            Action::Scroll { delta } => {
                finite(delta.x, "horizontal scroll")?;
                finite(delta.y, "vertical scroll")?;
                Ok(Self::Scroll {
                    x: delta.x,
                    y: delta.y,
                })
            }
            _ => Err(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "this action has no virtual-pointer route".into(),
                retryable: false,
            }),
        }
    }
}

#[derive(Clone, Copy)]
struct PreparedPoint {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PreparedPoint {
    fn new(point: LogicalPoint, geometry: &DesktopGeometry) -> Result<Self, ProtocolError> {
        Ok(Self {
            x: geometry.normalise_x(point.x)?,
            y: geometry.normalise_y(point.y)?,
            width: geometry.width,
            height: geometry.height,
        })
    }
}

impl Drop for PointerState {
    fn drop(&mut self) {
        for button in std::mem::take(&mut self.held_buttons) {
            self.pointer
                .button(timestamp(), button, ButtonState::Released);
        }
        self.pointer.frame();
        self.pointer.destroy();
        let _ = self.connection.flush();
    }
}

#[derive(Default)]
struct WaylandState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

struct DesktopGeometry {
    left: f64,
    top: f64,
    width: u32,
    height: u32,
}

impl DesktopGeometry {
    fn inspect() -> Result<Self, ProtocolError> {
        let snapshot = NiriInspector
            .inspect(false)
            .map_err(|error| backend_failure(format!("inspect desktop geometry: {error}"), true))?;
        let first = snapshot
            .outputs
            .first()
            .ok_or_else(|| backend_failure("desktop has no active outputs", true))?;
        let mut left = first.logical_bounds.origin.x;
        let mut top = first.logical_bounds.origin.y;
        let mut right = left + first.logical_bounds.width;
        let mut bottom = top + first.logical_bounds.height;
        for output in &snapshot.outputs[1..] {
            left = left.min(output.logical_bounds.origin.x);
            top = top.min(output.logical_bounds.origin.y);
            right = right.max(output.logical_bounds.origin.x + output.logical_bounds.width);
            bottom = bottom.max(output.logical_bounds.origin.y + output.logical_bounds.height);
        }
        Ok(Self {
            left,
            top,
            width: extent(right - left, "desktop width")?,
            height: extent(bottom - top, "desktop height")?,
        })
    }

    fn normalise_x(&self, value: f64) -> Result<u32, ProtocolError> {
        coordinate(value - self.left, self.width, "pointer x")
    }

    fn normalise_y(&self, value: f64) -> Result<u32, ProtocolError> {
        coordinate(value - self.top, self.height, "pointer y")
    }
}

fn coordinate(value: f64, extent: u32, name: &str) -> Result<u32, ProtocolError> {
    finite(value, name)?;
    if value < 0.0 || value > f64::from(extent) {
        return Err(invalid(format!("{name} lies outside the desktop layout")));
    }
    Ok(value.round() as u32)
}

fn extent(value: f64, name: &str) -> Result<u32, ProtocolError> {
    finite(value, name)?;
    if value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(invalid(format!(
            "{name} cannot be represented by the pointer protocol"
        )));
    }
    Ok(value.ceil() as u32)
}

fn finite(value: f64, name: &str) -> Result<(), ProtocolError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(invalid(format!("{name} must be finite")))
    }
}

fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Primary => BTN_LEFT,
        PointerButton::Secondary => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
        PointerButton::Back => BTN_SIDE,
        PointerButton::Forward => BTN_EXTRA,
    }
}

fn timestamp() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

fn sleep_cancellable(
    duration: Duration,
    cancellation: &ExecutionToken,
    dispatched: bool,
) -> Result<(), ActionExecutionFailure> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        cancellation_boundary(cancellation, dispatched)?;
        let step = remaining.min(PATH_STEP);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    Ok(())
}

fn cancelled_before_dispatch(cancellation: &ExecutionToken) -> Result<(), ActionExecutionFailure> {
    if cancellation.is_cancelled() {
        Err(ActionExecutionFailure::not_executed(cancelled()))
    } else {
        Ok(())
    }
}

fn cancellation_boundary(
    cancellation: &ExecutionToken,
    dispatched: bool,
) -> Result<(), ActionExecutionFailure> {
    if !cancellation.is_cancelled() {
        return Ok(());
    }
    if dispatched {
        Err(ActionExecutionFailure::indeterminate(
            ActionRoute::VirtualPointer,
            ProtocolError {
                code: ErrorCode::Cancelled,
                message: "the pointer action was cancelled after partial dispatch".into(),
                retryable: false,
            },
        ))
    } else {
        Err(ActionExecutionFailure::not_executed(cancelled()))
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: message.into(),
        retryable: false,
    }
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled before virtual-pointer dispatch".into(),
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
    use super::{DesktopGeometry, button_code};
    use waldo_protocol::PointerButton;

    #[test]
    fn desktop_coordinates_are_normalised_from_negative_origins() {
        let geometry = DesktopGeometry {
            left: -1920.0,
            top: -100.0,
            width: 4480,
            height: 1540,
        };

        assert_eq!(geometry.normalise_x(-1920.0).unwrap(), 0);
        assert_eq!(geometry.normalise_x(2560.0).unwrap(), 4480);
        assert_eq!(geometry.normalise_y(0.0).unwrap(), 100);
        assert!(geometry.normalise_x(2561.0).is_err());
    }

    #[test]
    fn semantic_buttons_map_to_linux_input_codes() {
        assert_eq!(button_code(PointerButton::Primary), 0x110);
        assert_eq!(button_code(PointerButton::Secondary), 0x111);
        assert_eq!(button_code(PointerButton::Middle), 0x112);
        assert_eq!(button_code(PointerButton::Back), 0x113);
        assert_eq!(button_code(PointerButton::Forward), 0x114);
    }
}
