//! Focused-window AT-SPI observation and deterministic element actions.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use atspi_common::{
    Action as AtspiAction, CoordType, Interface, InterfaceSet, ObjectRefOwned, StateSet,
};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{
    AccessibilityNode, AccessibilityOperation, Action, ActionRoute, ElementHandle, ErrorCode,
    LogicalPoint, LogicalRect, ProtocolError, WindowId,
};
use zbus::blocking::{Connection, Proxy, connection::Builder as ConnectionBuilder};

const MAX_NODES: usize = 256;
const MAX_TEXT_CHARACTERS: usize = 512;
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const ACTION: &str = "org.a11y.atspi.Action";
const COMPONENT: &str = "org.a11y.atspi.Component";
const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
const SELECTION: &str = "org.a11y.atspi.Selection";
const TEXT: &str = "org.a11y.atspi.Text";
const VALUE: &str = "org.a11y.atspi.Value";

/// A compact snapshot plus its focused element, if AT-SPI reports one.
pub struct AccessibilitySnapshot {
    pub nodes: Vec<AccessibilityNode>,
    pub focused: Option<ElementHandle>,
}

/// Shared AT-SPI authority whose handles remain opaque outside the process.
pub struct AccessibilityBackend {
    state: Mutex<BackendState>,
}

struct BackendState {
    connection: Connection,
    next_handle: u64,
    handles: BTreeMap<ElementHandle, ObjectKey>,
    reverse_handles: BTreeMap<ObjectKey, ElementHandle>,
    focused: Option<ElementHandle>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ObjectKey {
    name: String,
    path: String,
}

struct RawNode {
    role: String,
    name: Option<String>,
    value: Option<String>,
    text: Option<String>,
    states: Vec<String>,
    bounds: Option<LogicalRect>,
    actions: Vec<String>,
    children: Vec<ObjectKey>,
    focused: bool,
}

impl AccessibilityBackend {
    /// Connects to the accessibility bus and proves its registry is queryable.
    pub fn initialise() -> Result<Self, ProtocolError> {
        let session = Connection::session()
            .map_err(|error| backend_failure(format!("connect to session D-Bus: {error}"), true))?;
        let bus = Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .map_err(|error| backend_failure(format!("open AT-SPI bus proxy: {error}"), false))?;
        let address: String = bus
            .call("GetAddress", &())
            .map_err(|error| backend_failure(format!("read AT-SPI bus address: {error}"), true))?;
        let connection = ConnectionBuilder::address(address.as_str())
            .and_then(|builder| builder.build())
            .map_err(|error| backend_failure(format!("connect to AT-SPI bus: {error}"), true))?;
        registry_children(&connection)?;

        Ok(Self {
            state: Mutex::new(BackendState {
                connection,
                next_handle: 1,
                handles: BTreeMap::new(),
                reverse_handles: BTreeMap::new(),
                focused: None,
            }),
        })
    }

    /// Returns semantics only for the Niri window whose process owns the frame.
    pub fn snapshot(
        &self,
        pid: u32,
        window_id: WindowId,
        application_id: Option<String>,
        window_title: Option<String>,
        visible_bounds: LogicalRect,
    ) -> Result<AccessibilitySnapshot, ProtocolError> {
        let mut state = self.state.lock().map_err(|_| {
            backend_failure(
                "accessibility handle registry is poisoned".to_owned(),
                false,
            )
        })?;
        let connection = state.connection.clone();
        let mut application_roots = Vec::new();
        for reference in registry_children(&connection)? {
            let Some(key) = object_key(&reference) else {
                continue;
            };
            if peer_pid(&connection, &key.name)? == pid {
                application_roots.push(key);
            }
        }
        if application_roots.is_empty() {
            return Ok(AccessibilitySnapshot {
                nodes: Vec::new(),
                focused: None,
            });
        }

        let mut top_levels = Vec::new();
        for application_root in application_roots {
            let Ok(application) = read_node(&connection, &application_root) else {
                continue;
            };
            for child in application.children {
                if let Ok(raw) = read_node(&connection, &child) {
                    top_levels.push((child, raw));
                }
            }
        }
        let Some((root, root_node)) =
            select_window_root(top_levels, window_title.as_deref(), visible_bounds)
        else {
            return Ok(AccessibilitySnapshot {
                nodes: Vec::new(),
                focused: None,
            });
        };
        let screen_coordinates = root_node
            .bounds
            .is_some_and(|bounds| points_near(bounds.origin, visible_bounds.origin, 8.0));
        let application = application_id.unwrap_or_else(|| "accessible application".into());
        let mut queue = VecDeque::from([(root, Some(root_node), true)]);
        let mut nodes = Vec::new();
        let mut focused = None;
        while let Some((key, prefetched, is_root)) = queue.pop_front() {
            if nodes.len() >= MAX_NODES {
                break;
            }
            let raw = match prefetched.map_or_else(|| read_node(&connection, &key), Ok) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            queue.extend(
                raw.children
                    .iter()
                    .cloned()
                    .map(|child| (child, None, false)),
            );
            let handle = state.handle_for(key);
            let children = raw
                .children
                .into_iter()
                .map(|child| state.handle_for(child))
                .collect();
            if raw.focused {
                focused = Some(handle.clone());
            }
            let bounds = if is_root {
                Some(visible_bounds)
            } else if screen_coordinates {
                raw.bounds
                    .filter(|bounds| rectangles_intersect(*bounds, visible_bounds))
            } else {
                None
            };
            nodes.push(AccessibilityNode {
                handle,
                application: application.clone(),
                window_id: Some(window_id.clone()),
                role: raw.role,
                name: raw.name,
                value: raw.value,
                text: raw.text,
                states: raw.states,
                bounds,
                actions: raw.actions,
                children,
            });
        }

        state.focused = focused.clone();
        Ok(AccessibilitySnapshot { nodes, focused })
    }

    /// Checks whether a previously issued opaque handle still resolves.
    pub fn element_exists(&self, element: &ElementHandle) -> Result<bool, ProtocolError> {
        let Some((connection, key)) = self.resolved_element(element)? else {
            return Ok(false);
        };
        let accessible = proxy(&connection, &key, ACCESSIBLE)?;
        Ok(accessible.call::<_, _, String>("GetRoleName", &()).is_ok())
    }

    /// Checks a fresh AT-SPI state bit on a previously issued element.
    pub fn element_state(
        &self,
        element: &ElementHandle,
        expected: &str,
    ) -> Result<Option<bool>, ProtocolError> {
        let Some((connection, key)) = self.resolved_element(element)? else {
            return Ok(None);
        };
        let accessible = proxy(&connection, &key, ACCESSIBLE)?;
        let states: StateSet = match accessible.call("GetState", &()) {
            Ok(states) => states,
            Err(_) => return Ok(None),
        };
        Ok(Some(states.into_iter().any(|state| {
            serde_json::to_value(state)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .as_deref()
                == Some(expected)
        })))
    }

    fn resolved_element(
        &self,
        element: &ElementHandle,
    ) -> Result<Option<(Connection, ObjectKey)>, ProtocolError> {
        let state = self.state.lock().map_err(|_| {
            backend_failure(
                "accessibility handle registry is poisoned".to_owned(),
                false,
            )
        })?;
        Ok(state
            .handles
            .get(element)
            .cloned()
            .map(|key| (state.connection.clone(), key)))
    }

    /// Sets text on the focused element from the latest semantic observation.
    pub fn set_focused_text(
        &self,
        text: &str,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::Cancelled,
                message: "text entry was cancelled before AT-SPI dispatch".into(),
                retryable: false,
            }));
        }
        let element = self
            .state
            .lock()
            .map_err(|_| {
                ActionExecutionFailure::not_executed(backend_failure(
                    "accessibility handle registry is poisoned".to_owned(),
                    false,
                ))
            })?
            .focused
            .clone()
            .ok_or_else(|| {
                ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::UnsupportedCapability,
                    message: "the latest accessibility snapshot has no focused element".into(),
                    retryable: true,
                })
            })?;
        self.execute_accessibility(
            &element,
            &AccessibilityOperation::SetText {
                text: text.to_owned(),
            },
        )
    }

    fn execute_accessibility(
        &self,
        element: &ElementHandle,
        operation: &AccessibilityOperation,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let state = self.state.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(backend_failure(
                "accessibility handle registry is poisoned".to_owned(),
                false,
            ))
        })?;
        let key = state.handles.get(element).cloned().ok_or_else(|| {
            ActionExecutionFailure::not_executed(stale_target("accessibility handle is unknown"))
        })?;
        let connection = state.connection.clone();
        drop(state);
        let accessible =
            proxy(&connection, &key, ACCESSIBLE).map_err(ActionExecutionFailure::not_executed)?;
        accessible
            .call::<_, _, String>("GetRoleName", &())
            .map_err(|_| {
                ActionExecutionFailure::not_executed(stale_target(
                    "accessibility element no longer resolves",
                ))
            })?;
        let interfaces: InterfaceSet = accessible.call("GetInterfaces", &()).map_err(|error| {
            ActionExecutionFailure::not_executed(backend_failure(
                format!("read accessibility interfaces before action: {error}"),
                true,
            ))
        })?;

        match operation {
            AccessibilityOperation::Invoke { action } => {
                require_interface(&interfaces, Interface::Action, "actions")?;
                let proxy = proxy(&connection, &key, ACTION)
                    .map_err(ActionExecutionFailure::not_executed)?;
                let actions: Vec<AtspiAction> = proxy
                    .call("GetActions", &())
                    .map_err(|error| action_error("enumerate element actions", error))?;
                let index = actions
                    .iter()
                    .position(|candidate| candidate.name == *action)
                    .ok_or_else(|| {
                        ActionExecutionFailure::not_executed(ProtocolError {
                            code: ErrorCode::InvalidRequest,
                            message: format!(
                                "element has no accessibility action named {action:?}"
                            ),
                            retryable: false,
                        })
                    })?;
                require_true(
                    proxy.call("DoAction", &(index as i32)),
                    "invoke accessibility action",
                )?;
            }
            AccessibilityOperation::SetText { text } => {
                require_interface(&interfaces, Interface::EditableText, "editable text")?;
                let proxy = proxy(&connection, &key, EDITABLE_TEXT)
                    .map_err(ActionExecutionFailure::not_executed)?;
                require_true(
                    proxy.call("SetTextContents", &(text.as_str())),
                    "set accessibility text",
                )?;
            }
            AccessibilityOperation::SetValue { value } => {
                require_interface(&interfaces, Interface::Value, "numeric value")?;
                let value = value.parse::<f64>().map_err(|_| {
                    ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::InvalidRequest,
                        message: "accessibility value must be a finite number".into(),
                        retryable: false,
                    })
                })?;
                if !value.is_finite() {
                    return Err(ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::InvalidRequest,
                        message: "accessibility value must be a finite number".into(),
                        retryable: false,
                    }));
                }
                let proxy = proxy(&connection, &key, VALUE)
                    .map_err(ActionExecutionFailure::not_executed)?;
                proxy
                    .call::<_, _, ()>("SetCurrentValue", &value)
                    .map_err(|error| action_error("set accessibility value", error))?;
            }
            AccessibilityOperation::Focus => {
                require_interface(&interfaces, Interface::Component, "component focus")?;
                let proxy = proxy(&connection, &key, COMPONENT)
                    .map_err(ActionExecutionFailure::not_executed)?;
                require_true(proxy.call("GrabFocus", &()), "focus accessibility element")?;
            }
            AccessibilityOperation::Select => {
                require_interface(&interfaces, Interface::Selection, "selection")?;
                let parent: ObjectRefOwned = accessible
                    .get_property("Parent")
                    .map_err(|error| action_error("resolve selection parent", error))?;
                let Some(parent) = object_key(&parent) else {
                    return Err(ActionExecutionFailure::not_executed(stale_target(
                        "accessibility element has no selectable parent",
                    )));
                };
                let index: i32 = accessible
                    .call("GetIndexInParent", &())
                    .map_err(|error| action_error("resolve selection index", error))?;
                let proxy = proxy(&connection, &parent, SELECTION)
                    .map_err(ActionExecutionFailure::not_executed)?;
                require_true(
                    proxy.call("SelectChild", &index),
                    "select accessibility element",
                )?;
            }
        }
        Ok(ActionRoute::Accessibility.into())
    }
}

impl BackendState {
    fn handle_for(&mut self, key: ObjectKey) -> ElementHandle {
        if let Some(handle) = self.reverse_handles.get(&key) {
            return handle.clone();
        }
        let handle = ElementHandle(format!("element-{}", self.next_handle));
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .expect("accessibility handle space exhausted");
        self.handles.insert(handle.clone(), key.clone());
        self.reverse_handles.insert(key, handle.clone());
        handle
    }
}

impl ActionExecutor for AccessibilityBackend {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::Cancelled,
                message: "the action batch was cancelled before accessibility dispatch".into(),
                retryable: false,
            }));
        }
        let Action::Accessibility { element, operation } = action else {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "this action has no accessibility route".into(),
                retryable: false,
            }));
        };
        self.execute_accessibility(element, operation)
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::Accessibility
    }
}

fn select_window_root(
    mut candidates: Vec<(ObjectKey, RawNode)>,
    window_title: Option<&str>,
    visible_bounds: LogicalRect,
) -> Option<(ObjectKey, RawNode)> {
    if let Some(title) = window_title {
        let title_matches: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, (_, node))| (node.name.as_deref() == Some(title)).then_some(index))
            .collect();
        if let [index] = title_matches.as_slice() {
            return Some(candidates.swap_remove(*index));
        }
    }

    let size_matches: Vec<_> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, (_, node))| {
            node.bounds
                .is_some_and(|bounds| sizes_near(bounds, visible_bounds, 8.0))
                .then_some(index)
        })
        .collect();
    if let [index] = size_matches.as_slice() {
        return Some(candidates.swap_remove(*index));
    }

    (candidates.len() == 1).then(|| candidates.pop().expect("one candidate remains"))
}

fn points_near(left: LogicalPoint, right: LogicalPoint, tolerance: f64) -> bool {
    (left.x - right.x).abs() <= tolerance && (left.y - right.y).abs() <= tolerance
}

fn sizes_near(left: LogicalRect, right: LogicalRect, tolerance: f64) -> bool {
    (left.width - right.width).abs() <= tolerance && (left.height - right.height).abs() <= tolerance
}

fn registry_children(connection: &Connection) -> Result<Vec<ObjectRefOwned>, ProtocolError> {
    proxy(
        connection,
        &ObjectKey {
            name: "org.a11y.atspi.Registry".into(),
            path: "/org/a11y/atspi/accessible/root".into(),
        },
        ACCESSIBLE,
    )?
    .call("GetChildren", &())
    .map_err(|error| backend_failure(format!("enumerate AT-SPI applications: {error}"), true))
}

fn peer_pid(connection: &Connection, name: &str) -> Result<u32, ProtocolError> {
    Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|error| backend_failure(format!("open AT-SPI D-Bus proxy: {error}"), false))?
    .call("GetConnectionUnixProcessID", &(name))
    .map_err(|error| backend_failure(format!("resolve AT-SPI peer process: {error}"), true))
}

fn read_node(connection: &Connection, key: &ObjectKey) -> Result<RawNode, ProtocolError> {
    let accessible = proxy(connection, key, ACCESSIBLE)?;
    let interfaces: InterfaceSet = accessible.call("GetInterfaces", &()).map_err(|error| {
        backend_failure(format!("read accessibility interfaces: {error}"), true)
    })?;
    let states: StateSet = accessible
        .call("GetState", &())
        .map_err(|error| backend_failure(format!("read accessibility states: {error}"), true))?;
    let focused = states.contains(atspi_common::State::Focused);
    let states = states
        .into_iter()
        .filter_map(|state| {
            serde_json::to_value(state)
                .ok()?
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    let children: Vec<ObjectRefOwned> = accessible.call("GetChildren", &()).unwrap_or_default();
    let children = children.iter().filter_map(object_key).collect();
    let role = accessible
        .call("GetRoleName", &())
        .unwrap_or_else(|_| "unknown".into());
    let name = non_empty(accessible.get_property("Name").unwrap_or_default());
    let bounds = interfaces
        .contains(Interface::Component)
        .then(|| {
            proxy(connection, key, COMPONENT)
                .ok()?
                .call::<_, _, (i32, i32, i32, i32)>("GetExtents", &CoordType::Screen)
                .ok()
        })
        .flatten()
        .and_then(|(x, y, width, height)| {
            (width > 0 && height > 0).then_some(LogicalRect {
                origin: LogicalPoint {
                    x: f64::from(x),
                    y: f64::from(y),
                },
                width: f64::from(width),
                height: f64::from(height),
            })
        });
    let text = interfaces
        .contains(Interface::Text)
        .then(|| {
            proxy(connection, key, TEXT)
                .ok()?
                .call::<_, _, String>("GetText", &(0_i32, -1_i32))
                .ok()
        })
        .flatten()
        .and_then(non_empty)
        .map(|text| text.chars().take(MAX_TEXT_CHARACTERS).collect());
    let value = interfaces
        .contains(Interface::Value)
        .then(|| {
            proxy(connection, key, VALUE)
                .ok()?
                .get_property::<f64>("CurrentValue")
                .map(|value| value.to_string())
                .ok()
        })
        .flatten();
    let actions = interfaces
        .contains(Interface::Action)
        .then(|| {
            proxy(connection, key, ACTION)
                .ok()?
                .call::<_, _, Vec<AtspiAction>>("GetActions", &())
                .map(|actions| actions.into_iter().map(|action| action.name).collect())
                .ok()
        })
        .flatten()
        .unwrap_or_default();

    Ok(RawNode {
        role,
        name,
        value,
        text,
        states,
        bounds,
        actions,
        children,
        focused,
    })
}

fn proxy<'a>(
    connection: &'a Connection,
    key: &'a ObjectKey,
    interface: &'static str,
) -> Result<Proxy<'a>, ProtocolError> {
    Proxy::new(connection, key.name.as_str(), key.path.as_str(), interface)
        .map_err(|error| backend_failure(format!("open accessibility object proxy: {error}"), true))
}

fn object_key(reference: &ObjectRefOwned) -> Option<ObjectKey> {
    Some(ObjectKey {
        name: reference.name()?.as_str().to_owned(),
        path: reference.path().as_str().to_owned(),
    })
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn rectangles_intersect(left: LogicalRect, right: LogicalRect) -> bool {
    left.origin.x < right.origin.x + right.width
        && left.origin.x + left.width > right.origin.x
        && left.origin.y < right.origin.y + right.height
        && left.origin.y + left.height > right.origin.y
}

fn require_true(
    result: Result<bool, zbus::Error>,
    operation: &str,
) -> Result<(), ActionExecutionFailure> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::BackendFailure,
            message: format!("AT-SPI refused to {operation}"),
            retryable: false,
        })),
        Err(error) => Err(action_error(operation, error)),
    }
}

fn require_interface(
    interfaces: &InterfaceSet,
    interface: Interface,
    operation: &str,
) -> Result<(), ActionExecutionFailure> {
    if interfaces.contains(interface) {
        Ok(())
    } else {
        Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: format!("accessibility element does not expose {operation}"),
            retryable: false,
        }))
    }
}

fn action_error(operation: &str, error: zbus::Error) -> ActionExecutionFailure {
    ActionExecutionFailure::indeterminate(
        ActionRoute::Accessibility,
        backend_failure(format!("could not {operation}: {error}"), true),
    )
}

fn stale_target(message: &str) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::StaleAccessibilityTarget,
        message: message.into(),
        retryable: true,
    }
}

fn backend_failure(message: String, retryable: bool) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{LogicalPoint, LogicalRect};

    use super::{ObjectKey, RawNode, rectangles_intersect, select_window_root};

    #[test]
    fn semantic_bounds_are_filtered_to_the_observed_frame() {
        let frame = LogicalRect {
            origin: LogicalPoint { x: 100.0, y: 50.0 },
            width: 200.0,
            height: 100.0,
        };
        assert!(rectangles_intersect(
            frame,
            LogicalRect {
                origin: LogicalPoint { x: 150.0, y: 75.0 },
                width: 10.0,
                height: 10.0,
            }
        ));
        assert!(!rectangles_intersect(
            frame,
            LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 50.0,
                height: 50.0,
            }
        ));
    }

    #[test]
    fn exact_window_title_selects_one_top_level_from_a_shared_process() {
        let bounds = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 300.0,
            height: 200.0,
        };
        let selected = select_window_root(
            vec![
                candidate("/first", "Other window", bounds),
                candidate("/second", "Target window", bounds),
            ],
            Some("Target window"),
            LogicalRect {
                origin: LogicalPoint {
                    x: 1000.0,
                    y: 200.0,
                },
                ..bounds
            },
        )
        .expect("title should identify one top-level window");

        assert_eq!(selected.0.path, "/second");
    }

    fn candidate(path: &str, name: &str, bounds: LogicalRect) -> (ObjectKey, RawNode) {
        (
            ObjectKey {
                name: ":1.42".into(),
                path: path.into(),
            },
            RawNode {
                role: "dialog".into(),
                name: Some(name.into()),
                value: None,
                text: None,
                states: Vec::new(),
                bounds: Some(bounds),
                actions: Vec::new(),
                children: Vec::new(),
                focused: false,
            },
        )
    }
}
