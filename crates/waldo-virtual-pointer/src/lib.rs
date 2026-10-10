//! Wayland virtual-pointer execution with held-button containment.

use std::{collections::BTreeSet, sync::Mutex, thread, time::Duration};

use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor, input_event_time_ms};
use waldo_controller::ExecutionToken;
use waldo_niri::NiriInspector;
use waldo_protocol::{
    Action, ActionRoute, ErrorCode, InputState, LogicalPoint, OutputTransform, PointerButton,
    ProtocolError,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_output,
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
/// Longest drag accepted. A drag's path is prepared before dispatch, one
/// point per step, so the bound keeps a long duration from exhausting memory.
const MAX_DRAG_DURATION_MS: u32 = 60_000;
/// Largest magnitude a Wayland fixed-point (signed 24.8) argument carries.
const MAX_FIXED_MAGNITUDE: f64 = 8_388_607.0;
/// The first wl_output version that reports output names, which every
/// output lookup here depends on.
const OUTPUT_NAME_VERSION: u32 = 4;

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

    fn release_all(&self) -> Option<String> {
        // Destroying the virtual device is the compositor-owned release-all
        // boundary, including after an interrupted explicit press; a poisoned
        // lock still holds the device, so recover the guard to drop it.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        None
    }
}

struct PointerState {
    connection: Connection,
    globals: GlobalList,
    queue: EventQueue<WaylandState>,
    manager: ZwlrVirtualPointerManagerV1,
    pointer: ZwlrVirtualPointerV1,
    pointer_output: Option<String>,
    outputs: Vec<OutputRecord>,
    held_buttons: BTreeSet<u32>,
}

impl PointerState {
    fn connect() -> Result<Self, ProtocolError> {
        let connection = Connection::connect_to_env()
            .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
        let (globals, mut queue) = registry_queue_init::<WaylandState>(&connection)
            .map_err(|error| backend_failure(format!("read Wayland globals: {error}"), true))?;
        let qh = queue.handle();
        // Version 2 is the first that can bind a pointer to an output.
        let manager: ZwlrVirtualPointerManagerV1 =
            globals.bind(&qh, 2..=2, ()).map_err(|error| {
                backend_failure(
                    format!("bind virtual-pointer manager version 2: {error}"),
                    false,
                )
            })?;
        let mut wayland_state = WaylandState::default();
        let mut too_old = None;
        globals.contents().with_list(|list| {
            for global in list.iter().filter(|global| global.interface == "wl_output") {
                if global.version < OUTPUT_NAME_VERSION {
                    too_old = Some(global.version);
                    continue;
                }
                let output = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    global.name,
                    OUTPUT_NAME_VERSION,
                    &qh,
                    (),
                );
                wayland_state.outputs.push(OutputRecord {
                    output,
                    global: global.name,
                    name: None,
                });
            }
        });
        if let Some(version) = too_old {
            return Err(backend_failure(
                format!(
                    "the compositor offers wl_output version {version}; the virtual pointer needs version {OUTPUT_NAME_VERSION} for output names"
                ),
                false,
            ));
        }
        queue.roundtrip(&mut wayland_state).map_err(|error| {
            backend_failure(format!("read Wayland output names: {error}"), true)
        })?;
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        connection
            .flush()
            .map_err(|error| backend_failure(format!("create virtual pointer: {error}"), true))?;
        Ok(Self {
            connection,
            globals,
            queue,
            manager,
            pointer,
            pointer_output: None,
            outputs: wayland_state.outputs,
            held_buttons: BTreeSet::new(),
        })
    }

    fn execute(
        &mut self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<(), ActionExecutionFailure> {
        self.refresh_output_inventory()?;
        let prepared =
            PreparedAction::from_action(action).map_err(ActionExecutionFailure::not_executed)?;
        cancelled_before_dispatch(cancellation)?;

        match prepared {
            PreparedAction::MoveAbsolute(point) => self.send_absolute(&point)?,
            PreparedAction::MoveRelative { x, y } => {
                self.pointer.motion(input_event_time_ms(), x, y);
                self.pointer.frame();
            }
            PreparedAction::Button { button, state } => self.button(button, state),
            PreparedAction::Click {
                point,
                button,
                count,
            } => {
                self.send_absolute(&point)?;
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
                self.send_absolute(first)?;
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
                let time = input_event_time_ms();
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
            .dispatch_pending(&mut WaylandState::default())
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

    fn refresh_output_inventory(&mut self) -> Result<(), ActionExecutionFailure> {
        if self.held_buttons.is_empty() {
            // A fresh device binds the compositor's current wl_output objects, including
            // remove-and-readd mutations which reuse an output name.
            *self = Self::connect().map_err(ActionExecutionFailure::not_executed)?;
            return Ok(());
        }

        // While a button is held the device and its outputs are kept, so
        // compare the registry's output globals, which an output removed and
        // re-added under the same name does not keep.
        self.queue
            .roundtrip(&mut WaylandState::default())
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::VirtualPointer,
                    backend_failure(format!("read Wayland output changes: {error}"), true),
                )
            })?;
        let known = self
            .outputs
            .iter()
            .map(|output| output.global)
            .collect::<BTreeSet<_>>();
        let live = self.globals.contents().with_list(|list| {
            list.iter()
                .filter(|global| global.interface == "wl_output")
                .map(|global| global.name)
                .collect::<BTreeSet<_>>()
        });
        if known == live {
            return Ok(());
        }

        let refreshed = Self::connect().map_err(ActionExecutionFailure::not_executed)?;
        *self = refreshed;
        Err(ActionExecutionFailure::indeterminate(
            ActionRoute::VirtualPointer,
            backend_failure(
                "output topology changed while a pointer button was held; Waldo released the virtual device before refusing further dispatch",
                true,
            ),
        ))
    }

    fn send_absolute(&mut self, point: &PreparedPoint) -> Result<(), ActionExecutionFailure> {
        self.select_output(&point.output)?;
        self.pointer.motion_absolute(
            input_event_time_ms(),
            point.x,
            point.y,
            point.width,
            point.height,
        );
        self.pointer.frame();
        Ok(())
    }

    fn select_output(&mut self, output_name: &str) -> Result<(), ActionExecutionFailure> {
        if self.pointer_output.as_deref() == Some(output_name) {
            return Ok(());
        }
        if !self.held_buttons.is_empty() {
            return Err(ActionExecutionFailure::indeterminate(
                ActionRoute::VirtualPointer,
                invalid("a pointer drag cannot cross an output boundary"),
            ));
        }
        let output = self
            .outputs
            .iter()
            .find(|output| output.name.as_deref() == Some(output_name))
            .map(|output| output.output.clone())
            .ok_or_else(|| {
                ActionExecutionFailure::not_executed(backend_failure(
                    format!("Wayland output {output_name} is no longer available"),
                    true,
                ))
            })?;
        self.pointer.destroy();
        self.pointer = self.manager.create_virtual_pointer_with_output(
            None,
            Some(&output),
            &self.queue.handle(),
            (),
        );
        self.pointer_output = Some(output_name.to_owned());
        Ok(())
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
        self.pointer
            .button(input_event_time_ms(), code, wayland_state);
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
            self.send_absolute(point)?;
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
    MoveAbsolute(PreparedPoint),
    MoveRelative {
        x: f64,
        y: f64,
    },
    Button {
        button: PointerButton,
        state: InputState,
    },
    Click {
        point: PreparedPoint,
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
                Ok(Self::MoveAbsolute(point))
            }
            Action::PointerMoveRelative { delta } => {
                wire_fixed(delta.x, "relative pointer x")?;
                wire_fixed(delta.y, "relative pointer y")?;
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
                    point,
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
                if *duration_ms > MAX_DRAG_DURATION_MS {
                    return Err(invalid(
                        "pointer drag duration must be at most 60000 milliseconds",
                    ));
                }
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
                wire_fixed(delta.x, "horizontal scroll")?;
                wire_fixed(delta.y, "vertical scroll")?;
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

#[derive(Clone)]
struct PreparedPoint {
    output: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PreparedPoint {
    fn new(point: LogicalPoint, geometry: &DesktopGeometry) -> Result<Self, ProtocolError> {
        let output = geometry.output_at(point).ok_or_else(|| {
            invalid("pointer position lies outside every active output in the desktop layout")
        })?;
        let bounds = output.logical_bounds;
        let (width, height) = (bounds.width, bounds.height);
        let (x, y) = (point.x - bounds.origin.x, point.y - bounds.origin.y);
        // Niri applies the output transform to output-bound absolute input.
        // Send its inverse in the untransformed output extent.
        let (x, y, width, height) = match output.transform {
            OutputTransform::Normal => (x, y, width, height),
            OutputTransform::Rotate90 => (y, width - x, height, width),
            OutputTransform::Rotate180 => (width - x, height - y, width, height),
            OutputTransform::Rotate270 => (height - y, x, height, width),
            OutputTransform::Flipped => (width - x, y, width, height),
            OutputTransform::Flipped90 => (y, x, height, width),
            OutputTransform::Flipped180 => (x, height - y, width, height),
            OutputTransform::Flipped270 => (height - y, width - x, height, width),
        };
        let width = extent(width, "output width")?;
        let height = extent(height, "output height")?;
        Ok(Self {
            output: output.name.clone(),
            x: coordinate(x, width, "pointer x")?,
            y: coordinate(y, height, "pointer y")?,
            width,
            height,
        })
    }
}

impl Drop for PointerState {
    fn drop(&mut self) {
        for button in std::mem::take(&mut self.held_buttons) {
            self.pointer
                .button(input_event_time_ms(), button, ButtonState::Released);
        }
        self.pointer.frame();
        self.pointer.destroy();
        let _ = self.connection.flush();
    }
}

#[derive(Default)]
struct WaylandState {
    outputs: Vec<OutputRecord>,
}

struct OutputRecord {
    output: wl_output::WlOutput,
    /// The registry global the output was bound from; a re-added output gets
    /// a new one even when its name is unchanged.
    global: u32,
    name: Option<String>,
}

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

impl Dispatch<wl_output::WlOutput, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(record) = state.outputs.iter_mut().find(|item| item.output == *proxy)
        {
            record.name = Some(name);
        }
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
    outputs: Vec<waldo_protocol::OutputState>,
}

impl DesktopGeometry {
    fn inspect() -> Result<Self, ProtocolError> {
        let snapshot = NiriInspector
            .inspect(false)
            .map_err(|error| backend_failure(format!("inspect desktop geometry: {error}"), true))?;
        if snapshot.outputs.is_empty() {
            return Err(backend_failure("desktop has no active outputs", true));
        }
        Ok(Self {
            outputs: snapshot.outputs,
        })
    }

    fn output_at(&self, point: LogicalPoint) -> Option<&waldo_protocol::OutputState> {
        self.outputs
            .iter()
            .find(|output| output.logical_bounds.contains(point))
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

/// Refuses a value a Wayland fixed-point argument cannot carry, which
/// would otherwise be sent as a different, saturated value.
fn wire_fixed(value: f64, name: &str) -> Result<(), ProtocolError> {
    finite(value, name)?;
    if value.abs() > MAX_FIXED_MAGNITUDE {
        return Err(invalid(format!(
            "{name} must be within ±{MAX_FIXED_MAGNITUDE} logical pixels"
        )));
    }
    Ok(())
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
    use super::{DesktopGeometry, PreparedPoint, button_code};
    use waldo_protocol::{
        LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform, PointerButton,
    };

    #[test]
    fn motion_and_scroll_beyond_the_fixed_point_range_are_refused() {
        use waldo_protocol::{Action, LogicalDelta};

        let relative = |x| Action::PointerMoveRelative {
            delta: LogicalDelta { x, y: 0.0 },
        };
        let scroll = |y| Action::Scroll {
            delta: LogicalDelta { x: 0.0, y },
        };

        assert!(super::PreparedAction::from_action(&relative(8_388_607.0)).is_ok());
        assert!(super::PreparedAction::from_action(&relative(1e100)).is_err());
        assert!(super::PreparedAction::from_action(&scroll(-1e100)).is_err());
        assert!(super::PreparedAction::from_action(&scroll(f64::NAN)).is_err());
    }

    #[test]
    fn an_overlong_drag_is_refused_before_its_path_is_prepared() {
        let drag = waldo_protocol::Action::PointerDrag {
            from: LogicalPoint { x: 0.0, y: 0.0 },
            to: LogicalPoint { x: 10.0, y: 10.0 },
            button: PointerButton::Primary,
            duration_ms: u32::MAX,
        };

        let Err(error) = super::PreparedAction::from_action(&drag) else {
            panic!("an overlong drag was prepared")
        };
        assert!(error.message.contains("duration"), "{}", error.message);
    }

    #[test]
    fn absolute_points_undo_the_output_transform_before_dispatch() {
        let cases = [
            (OutputTransform::Normal, (100, 60, 800, 600)),
            (OutputTransform::Rotate90, (60, 700, 600, 800)),
            (OutputTransform::Rotate180, (700, 540, 800, 600)),
            (OutputTransform::Rotate270, (540, 100, 600, 800)),
            (OutputTransform::Flipped, (700, 60, 800, 600)),
            (OutputTransform::Flipped90, (60, 100, 600, 800)),
            (OutputTransform::Flipped180, (100, 540, 800, 600)),
            (OutputTransform::Flipped270, (540, 700, 600, 800)),
        ];
        for (transform, expected) in cases {
            let mut output = output("transformed", -1200.0, 300.0, 800.0, 600.0);
            output.transform = transform;
            output.scale = 1.5;
            let geometry = DesktopGeometry {
                outputs: vec![output],
            };
            let point = PreparedPoint::new(
                LogicalPoint {
                    x: -1100.0,
                    y: 360.0,
                },
                &geometry,
            )
            .unwrap();
            assert_eq!(
                (point.x, point.y, point.width, point.height),
                expected,
                "{transform:?}"
            );
        }
    }

    #[test]
    fn desktop_coordinates_are_grounded_to_the_containing_output() {
        let geometry = DesktopGeometry {
            outputs: vec![
                output("left", -1920.0, -100.0, 1920.0, 1080.0),
                output("right", 0.0, 0.0, 2560.0, 1440.0),
            ],
        };

        let left = PreparedPoint::new(
            LogicalPoint {
                x: -960.0,
                y: 440.0,
            },
            &geometry,
        )
        .unwrap();
        assert_eq!(left.output, "left");
        assert_eq!(
            (left.x, left.y, left.width, left.height),
            (960, 540, 1920, 1080)
        );

        let right = PreparedPoint::new(
            LogicalPoint {
                x: 1280.0,
                y: 720.0,
            },
            &geometry,
        )
        .unwrap();
        assert_eq!(right.output, "right");
        assert_eq!(
            (right.x, right.y, right.width, right.height),
            (1280, 720, 2560, 1440)
        );

        assert!(PreparedPoint::new(LogicalPoint { x: 2560.0, y: 0.0 }, &geometry).is_err());
    }

    #[test]
    fn semantic_buttons_map_to_linux_input_codes() {
        assert_eq!(button_code(PointerButton::Primary), 0x110);
        assert_eq!(button_code(PointerButton::Secondary), 0x111);
        assert_eq!(button_code(PointerButton::Middle), 0x112);
        assert_eq!(button_code(PointerButton::Back), 0x113);
        assert_eq!(button_code(PointerButton::Forward), 0x114);
    }

    fn output(name: &str, x: f64, y: f64, width: f64, height: f64) -> OutputState {
        OutputState {
            id: OutputId(name.to_owned()),
            name: name.to_owned(),
            logical_bounds: LogicalRect {
                origin: LogicalPoint { x, y },
                width,
                height,
            },
            pixel_size: None,
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused: false,
        }
    }
}
