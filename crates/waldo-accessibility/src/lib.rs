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
    /// False when the walk stopped at the node budget, so an element absent
    /// from `nodes` may still exist and `focused` may be unknown.
    pub complete: bool,
}

/// Shared AT-SPI authority whose handles remain opaque outside the process.
pub struct AccessibilityBackend {
    state: Mutex<BackendState>,
}

struct BackendState {
    connection: Connection,
    registry: HandleRegistry,
    focused: Option<ElementHandle>,
}

/// The opaque handles issued for AT-SPI objects, each meaning the object
/// and identity it was issued for.
struct HandleRegistry {
    next_handle: u64,
    handles: BTreeMap<ElementHandle, ObservedElement>,
    reverse_handles: BTreeMap<ObjectKey, ElementHandle>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ObjectKey {
    name: String,
    path: String,
}

#[derive(Clone, Debug)]
struct ObservedElement {
    key: ObjectKey,
    window_root: ObjectKey,
    accessible_id: Option<String>,
}

struct RawNode {
    accessible_id: Option<String>,
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
                registry: HandleRegistry::new(),
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
        // Text entry relies on the focus this snapshot finds; until it
        // finishes, no earlier window's element stands in for it.
        state.focused = None;
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
                complete: true,
            });
        }

        let mut top_levels = Vec::new();
        for application_root in application_roots {
            let Some(application) = read_node(&connection, &application_root).or_gone()? else {
                continue;
            };
            for child in application.children {
                if let Some(raw) = read_node(&connection, &child).or_gone()? {
                    top_levels.push((child, raw));
                }
            }
        }
        let (root, root_node) =
            match select_window_root(top_levels, window_title.as_deref(), visible_bounds)? {
                Some(selected) => selected,
                None => {
                    return Ok(AccessibilitySnapshot {
                        nodes: Vec::new(),
                        focused: None,
                        complete: true,
                    });
                }
            };
        let screen_coordinates = root_node
            .bounds
            .is_some_and(|bounds| points_near(bounds.origin, visible_bounds.origin, 8.0));
        let application = application_id.unwrap_or_else(|| "accessible application".into());
        let window_root = root.clone();
        let mut queue = VecDeque::from([(root, Some(root_node), true)]);
        let mut nodes = Vec::new();
        let mut focused = None;
        let mut complete = true;
        while let Some((key, prefetched, is_root)) = queue.pop_front() {
            if nodes.len() >= MAX_NODES {
                complete = false;
                break;
            }
            let raw = match prefetched {
                Some(raw) => raw,
                // An element that vanished mid-walk is simply no longer
                // there; any other failure leaves the tree incomplete.
                None => match read_node(&connection, &key).or_gone()? {
                    Some(raw) => raw,
                    None => continue,
                },
            };
            queue.extend(
                raw.children
                    .iter()
                    .cloned()
                    .map(|child| (child, None, false)),
            );
            let handle =
                state
                    .registry
                    .handle_for(key, window_root.clone(), raw.accessible_id.clone());
            let children = raw
                .children
                .into_iter()
                .map(|child| state.registry.handle_for(child, window_root.clone(), None))
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
        Ok(AccessibilitySnapshot {
            nodes,
            focused,
            complete,
        })
    }

    /// Checks whether a previously issued opaque handle still resolves to
    /// the element it was issued for.
    pub fn element_exists(&self, element: &ElementHandle) -> Result<bool, ProtocolError> {
        Ok(self.resolved_element(element)?.is_some())
    }

    /// Checks a fresh AT-SPI state bit on a previously issued element;
    /// `None` when the element no longer resolves.
    pub fn element_state(
        &self,
        element: &ElementHandle,
        expected: &str,
    ) -> Result<Option<bool>, ProtocolError> {
        require_known_state(expected)?;
        let Some((connection, key)) = self.resolved_element(element)? else {
            return Ok(None);
        };
        let accessible = proxy(&connection, &key, ACCESSIBLE)?;
        let Some(states) = read_states(&accessible).or_gone()? else {
            return Ok(None);
        };
        Ok(Some(
            states
                .into_iter()
                .any(|state| state.to_static_str() == expected),
        ))
    }

    /// The element a handle was issued for, when its object still exists
    /// and still carries the identity it was observed with.
    fn resolved_element(
        &self,
        element: &ElementHandle,
    ) -> Result<Option<(Connection, ObjectKey)>, ProtocolError> {
        let (connection, observed) = {
            let state = self.state.lock().map_err(|_| {
                backend_failure(
                    "accessibility handle registry is poisoned".to_owned(),
                    false,
                )
            })?;
            let Some(observed) = state.registry.handles.get(element).cloned() else {
                return Ok(None);
            };
            (state.connection.clone(), observed)
        };
        Ok(
            object_matches_observed_identity(&connection, &observed.key, &observed.accessible_id)?
                .then_some((connection, observed.key)),
        )
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
        self.require_still_focused(&element)
            .map_err(ActionExecutionFailure::not_executed)?;
        self.execute_accessibility(
            &element,
            &AccessibilityOperation::SetText {
                text: text.to_owned(),
            },
        )
    }

    /// Refuses text entry into the cached focused element unless it still
    /// has focus and its window is still the active one: a batch may have
    /// moved focus since the snapshot that found it.
    fn require_still_focused(&self, element: &ElementHandle) -> Result<(), ProtocolError> {
        let (connection, observed) = {
            let state = self.state.lock().map_err(|_| {
                backend_failure(
                    "accessibility handle registry is poisoned".to_owned(),
                    false,
                )
            })?;
            let observed = state
                .registry
                .handles
                .get(element)
                .cloned()
                .ok_or_else(|| stale_target("the focused accessibility element was retired"))?;
            (state.connection.clone(), observed)
        };
        let still_focused =
            object_matches_observed_identity(&connection, &observed.key, &observed.accessible_id)?
                && read_states(&proxy(&connection, &observed.key, ACCESSIBLE)?)
                    .or_gone()?
                    .is_some_and(|states| states.contains(atspi_common::State::Focused));
        if !still_focused {
            return Err(stale_target(
                "the observed focused element no longer has focus; observe again",
            ));
        }
        let window_active = read_states(&proxy(&connection, &observed.window_root, ACCESSIBLE)?)
            .or_gone()?
            .is_some_and(|states| states.contains(atspi_common::State::Active));
        if !window_active {
            return Err(stale_target(
                "the observed focused element's window is no longer active; observe again",
            ));
        }
        Ok(())
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
        let observed = state
            .registry
            .handles
            .get(element)
            .cloned()
            .ok_or_else(|| {
                ActionExecutionFailure::not_executed(stale_target(
                    "accessibility handle is unknown",
                ))
            })?;
        let connection = state.connection.clone();
        drop(state);
        let key = self.resolve_action_element(
            element,
            observed,
            matches!(operation, AccessibilityOperation::SetValue { .. }),
        )?;
        let accessible =
            proxy(&connection, &key, ACCESSIBLE).map_err(ActionExecutionFailure::not_executed)?;
        confirm_resolved(accessible.call::<_, _, String>("GetRoleName", &()))
            .map_err(ActionExecutionFailure::not_executed)?;
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
                    .set_property("CurrentValue", value)
                    .map_err(|error| action_error("set accessibility value", error.into()))?;
            }
            AccessibilityOperation::Focus => {
                require_interface(&interfaces, Interface::Component, "component focus")?;
                let states: StateSet = accessible.call("GetState", &()).map_err(|error| {
                    ActionExecutionFailure::not_executed(backend_failure(
                        format!("read accessibility focus capability: {error}"),
                        false,
                    ))
                })?;
                if !states.contains(atspi_common::State::Focusable) {
                    return Err(ActionExecutionFailure::not_executed(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "accessibility element is not focusable".into(),
                        retryable: false,
                    }));
                }
                let proxy = proxy(&connection, &key, COMPONENT)
                    .map_err(ActionExecutionFailure::not_executed)?;
                focus_element(proxy.call("GrabFocus", &()))?;
                let states: StateSet = accessible.call("GetState", &()).map_err(|error| {
                    ActionExecutionFailure::indeterminate(
                        ActionRoute::Accessibility,
                        backend_failure(
                            format!("verify accessibility focus after dispatch: {error}"),
                            false,
                        ),
                    )
                })?;
                if !states.contains(atspi_common::State::Focused) {
                    return Err(ActionExecutionFailure::indeterminate(
                        ActionRoute::Accessibility,
                        ProtocolError {
                            code: ErrorCode::VerificationFailed,
                            message: "AT-SPI accepted focus without reporting the element focused"
                                .into(),
                            retryable: false,
                        },
                    ));
                }
                // Later text entry types into the element focus moved to.
                focus_cache(self.state.lock())?.focused = Some(element.clone());
            }
            AccessibilityOperation::Select => {
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
                if index < 0 {
                    return Err(ActionExecutionFailure::not_executed(stale_target(
                        "accessibility element has no current selection index",
                    )));
                }
                let parent_accessible = proxy(&connection, &parent, ACCESSIBLE)
                    .map_err(ActionExecutionFailure::not_executed)?;
                let parent_interfaces: InterfaceSet = parent_accessible
                    .call("GetInterfaces", &())
                    .map_err(|error| {
                        ActionExecutionFailure::not_executed(backend_failure(
                            format!("read selection parent interfaces: {error}"),
                            true,
                        ))
                    })?;
                require_interface(&parent_interfaces, Interface::Selection, "selection")?;
                let indexed_child: ObjectRefOwned = parent_accessible
                    .call("GetChildAtIndex", &index)
                    .map_err(|error| action_error("resolve indexed selection child", error))?;
                if object_key(&indexed_child).as_ref() != Some(&key) {
                    return Err(ActionExecutionFailure::not_executed(stale_target(
                        "accessibility selection parent no longer contains the observed child",
                    )));
                }
                let selection = proxy(&connection, &parent, SELECTION)
                    .map_err(ActionExecutionFailure::not_executed)?;
                require_true(
                    selection.call("SelectChild", &index),
                    "select accessibility element",
                )?;
                let selected: bool =
                    selection.call("IsChildSelected", &index).map_err(|error| {
                        ActionExecutionFailure::indeterminate(
                            ActionRoute::Accessibility,
                            backend_failure(
                                format!("verify selected accessibility child: {error}"),
                                false,
                            ),
                        )
                    })?;
                if !selected {
                    return Err(ActionExecutionFailure::indeterminate(
                        ActionRoute::Accessibility,
                        ProtocolError {
                            code: ErrorCode::VerificationFailed,
                            message: "AT-SPI accepted selection without selecting the child".into(),
                            retryable: false,
                        },
                    ));
                }
            }
        }
        Ok(ActionRoute::Accessibility.into())
    }

    fn resolve_action_element(
        &self,
        element: &ElementHandle,
        observed: ObservedElement,
        allow_identity_refresh: bool,
    ) -> Result<ObjectKey, ActionExecutionFailure> {
        let connection = self
            .state
            .lock()
            .map_err(|_| {
                ActionExecutionFailure::not_executed(backend_failure(
                    "accessibility handle registry is poisoned".to_owned(),
                    false,
                ))
            })?
            .connection
            .clone();
        let Some(accessible_id) = observed.accessible_id.as_deref() else {
            if object_matches_observed_identity(&connection, &observed.key, &None)
                .map_err(ActionExecutionFailure::not_executed)?
            {
                return Ok(observed.key);
            }
            return Err(ActionExecutionFailure::not_executed(stale_target(
                "accessibility element no longer resolves",
            )));
        };
        if !allow_identity_refresh {
            if object_matches_observed_identity(&connection, &observed.key, &observed.accessible_id)
                .map_err(ActionExecutionFailure::not_executed)?
            {
                return Ok(observed.key);
            }
            return Err(ActionExecutionFailure::not_executed(stale_target(
                "accessibility element no longer resolves",
            )));
        }
        let refreshed =
            find_unique_accessible_id(&connection, &observed.window_root, accessible_id)
                .map_err(ActionExecutionFailure::not_executed)?
                .ok_or_else(|| {
                    ActionExecutionFailure::not_executed(stale_target(
                        "accessibility element identity no longer resolves uniquely",
                    ))
                })?;
        let mut state = self.state.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(backend_failure(
                "accessibility handle registry is poisoned".to_owned(),
                false,
            ))
        })?;
        let previous_key = {
            let Some(current) = state.registry.handles.get_mut(element) else {
                return Err(ActionExecutionFailure::not_executed(stale_target(
                    "accessibility handle was retired during refresh",
                )));
            };
            if current.key != observed.key || current.accessible_id != observed.accessible_id {
                return Err(ActionExecutionFailure::not_executed(stale_target(
                    "accessibility handle changed during refresh",
                )));
            }
            let previous_key = current.key.clone();
            current.key = refreshed.clone();
            previous_key
        };
        state.registry.reverse_handles.remove(&previous_key);
        state
            .registry
            .reverse_handles
            .insert(refreshed.clone(), element.clone());
        Ok(refreshed)
    }
}

impl HandleRegistry {
    fn new() -> Self {
        Self {
            next_handle: 1,
            handles: BTreeMap::new(),
            reverse_handles: BTreeMap::new(),
        }
    }

    fn handle_for(
        &mut self,
        key: ObjectKey,
        window_root: ObjectKey,
        accessible_id: Option<String>,
    ) -> ElementHandle {
        if let Some(handle) = self.reverse_handles.get(&key).cloned() {
            let observed = self
                .handles
                .get_mut(&handle)
                .expect("every reverse handle names an issued handle");
            let same_identity = match (&observed.accessible_id, &accessible_id) {
                (Some(previous), Some(current)) => previous == current,
                _ => true,
            };
            if same_identity && observed.window_root == window_root {
                if accessible_id.is_some() {
                    observed.accessible_id = accessible_id;
                }
                return handle;
            }
            // The object path now names a different control. The old handle
            // keeps meaning the old control, so it is retired rather than
            // silently redirected.
            self.handles.remove(&handle);
            self.reverse_handles.remove(&key);
        }
        let handle = ElementHandle(format!("element-{}", self.next_handle));
        self.next_handle = self
            .next_handle
            .checked_add(1)
            .expect("accessibility handle space exhausted");
        self.handles.insert(
            handle.clone(),
            ObservedElement {
                key: key.clone(),
                window_root,
                accessible_id,
            },
        );
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
) -> Result<Option<(ObjectKey, RawNode)>, ProtocolError> {
    if let Some(title) = window_title {
        let title_matches: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, (_, node))| (node.name.as_deref() == Some(title)).then_some(index))
            .collect();
        if let [index] = title_matches.as_slice() {
            return Ok(Some(candidates.swap_remove(*index)));
        }
        if title_matches.len() > 1 {
            return Err(ambiguous_target(format!(
                "multiple AT-SPI windows from the focused process have title {title:?}"
            )));
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
        return Ok(Some(candidates.swap_remove(*index)));
    }
    if size_matches.len() > 1 {
        return Err(ambiguous_target(
            "multiple AT-SPI windows match the focused window geometry".into(),
        ));
    }

    match candidates.len() {
        0 => Ok(None),
        // The process's only accessible window is taken unless its title
        // contradicts the supplied one: a matching size would have selected
        // it above, so nothing else identifies it, and it may be another
        // window of the process than the one observed.
        1 => {
            let (_, node) = &candidates[0];
            let title_differs = window_title
                .zip(node.name.as_deref())
                .is_some_and(|(wanted, actual)| wanted != actual);
            Ok((!title_differs).then(|| candidates.pop()).flatten())
        }
        _ => Err(ambiguous_target(
            "multiple AT-SPI windows remain for the focused process".into(),
        )),
    }
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

/// Why reading an AT-SPI object failed.
enum ReadError {
    /// The object or its application no longer exists.
    Gone,
    /// The bus or the application failed; what was read is incomplete.
    Failed(ProtocolError),
}

trait OrGone<T> {
    /// A read whose object has gone yields nothing; any other failure
    /// stays an error.
    fn or_gone(self) -> Result<Option<T>, ProtocolError>;
}

impl<T> OrGone<T> for Result<T, ReadError> {
    fn or_gone(self) -> Result<Option<T>, ProtocolError> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(ReadError::Gone) => Ok(None),
            Err(ReadError::Failed(error)) => Err(error),
        }
    }
}

/// The D-Bus error name a failed call carries, when it carries one.
fn dbus_error_name(error: &zbus::Error) -> Option<String> {
    match error {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(error) => Some(zbus::DBusError::name(error.as_ref()).as_str().to_owned()),
        _ => None,
    }
}

fn object_gone(error: &zbus::Error) -> bool {
    matches!(
        dbus_error_name(error).as_deref(),
        Some(
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        )
    )
}

/// The object exists but does not offer the method or property asked for.
fn not_offered(error: &zbus::Error) -> bool {
    is_not_supported(error)
        || matches!(
            dbus_error_name(error).as_deref(),
            Some(
                "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownProperty"
                    | "org.freedesktop.DBus.Error.InvalidArgs"
            )
        )
}

/// A value the object must provide.
fn required<T>(result: Result<T, zbus::Error>, operation: &str) -> Result<T, ReadError> {
    result.map_err(|error| {
        if object_gone(&error) {
            ReadError::Gone
        } else {
            ReadError::Failed(backend_failure(format!("{operation}: {error}"), true))
        }
    })
}

/// A value the object may not offer: absent then, rather than an error.
fn optional<T>(result: Result<T, zbus::Error>, operation: &str) -> Result<Option<T>, ReadError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if not_offered(&error) => Ok(None),
        Err(error) => required(Err(error), operation),
    }
}

impl From<ProtocolError> for ReadError {
    fn from(error: ProtocolError) -> Self {
        Self::Failed(error)
    }
}

fn read_states(accessible: &Proxy<'_>) -> Result<StateSet, ReadError> {
    required(
        accessible.call("GetState", &()),
        "read accessibility states",
    )
}

/// Refuses a state name AT-SPI does not define, so a misspelt name cannot
/// make a negative state predicate pass.
pub fn require_known_state(name: &str) -> Result<(), ProtocolError> {
    let known = (0..u64::BITS)
        .filter_map(|bit| StateSet::from_bits(1_u64 << bit).ok())
        .flat_map(StateSet::iter)
        .any(|state| state.to_static_str() == name);
    if known {
        Ok(())
    } else {
        Err(ProtocolError {
            code: ErrorCode::InvalidRequest,
            message: format!("{name:?} is not an AT-SPI state name"),
            retryable: false,
        })
    }
}

fn read_node(connection: &Connection, key: &ObjectKey) -> Result<RawNode, ReadError> {
    let accessible = proxy(connection, key, ACCESSIBLE)?;
    let interfaces: InterfaceSet = required(
        accessible.call("GetInterfaces", &()),
        "read accessibility interfaces",
    )?;
    let states = read_states(&accessible)?;
    let focused = states.contains(atspi_common::State::Focused);
    let states = states
        .into_iter()
        .map(|state| state.to_static_str().to_owned())
        .collect();
    let children: Vec<ObjectRefOwned> = optional(
        accessible.call("GetChildren", &()),
        "read accessibility children",
    )?
    .unwrap_or_default();
    let children = children.iter().filter_map(object_key).collect();
    let role = optional(
        accessible.call("GetRoleName", &()),
        "read accessibility role",
    )?
    .unwrap_or_else(|| "unknown".into());
    let name =
        optional(accessible.get_property("Name"), "read accessibility name")?.and_then(non_empty);
    let accessible_id = optional(
        accessible.get_property("AccessibleId"),
        "read accessibility identity",
    )?
    .and_then(non_empty);
    let bounds = if interfaces.contains(Interface::Component) {
        optional(
            proxy(connection, key, COMPONENT)?
                .call::<_, _, (i32, i32, i32, i32)>("GetExtents", &CoordType::Screen),
            "read accessibility bounds",
        )?
    } else {
        None
    }
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
    let text = if interfaces.contains(Interface::Text) {
        optional(
            proxy(connection, key, TEXT)?.call::<_, _, String>("GetText", &(0_i32, -1_i32)),
            "read accessibility text",
        )?
    } else {
        None
    }
    .and_then(non_empty)
    .map(|text| text.chars().take(MAX_TEXT_CHARACTERS).collect());
    let value = if interfaces.contains(Interface::Value) {
        optional(
            proxy(connection, key, VALUE)?.get_property::<f64>("CurrentValue"),
            "read accessibility value",
        )?
        .map(|value| value.to_string())
    } else {
        None
    };
    let actions = if interfaces.contains(Interface::Action) {
        optional(
            proxy(connection, key, ACTION)?.call::<_, _, Vec<AtspiAction>>("GetActions", &()),
            "read accessibility actions",
        )?
        .map(|actions| actions.into_iter().map(|action| action.name).collect())
    } else {
        None
    }
    .unwrap_or_default();

    Ok(RawNode {
        accessible_id,
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

/// Whether the object still exists and, when an identity was observed,
/// still carries it. A gone object, or one whose identity can no longer
/// be read, does not match; a failed read is an error.
fn object_matches_observed_identity(
    connection: &Connection,
    key: &ObjectKey,
    accessible_id: &Option<String>,
) -> Result<bool, ProtocolError> {
    let accessible = proxy(connection, key, ACCESSIBLE)?;
    if required(
        accessible.call::<_, _, String>("GetRoleName", &()),
        "resolve accessibility element",
    )
    .or_gone()?
    .is_none()
    {
        return Ok(false);
    }
    let Some(expected) = accessible_id else {
        return Ok(true);
    };
    let actual = match optional(
        accessible.get_property::<String>("AccessibleId"),
        "read accessibility identity",
    )
    .or_gone()?
    {
        Some(actual) => actual,
        None => return Ok(false),
    };
    Ok(actual.as_ref() == Some(expected))
}

/// The one element below `window_root` carrying `accessible_id`, or `None`
/// when there is none or more than one. Any branch that could not be read
/// is an error, since it could hold a second match.
fn find_unique_accessible_id(
    connection: &Connection,
    window_root: &ObjectKey,
    accessible_id: &str,
) -> Result<Option<ObjectKey>, ProtocolError> {
    let mut queue = VecDeque::from([window_root.clone()]);
    let mut matched = None;
    let mut visited = 0;
    while let Some(key) = queue.pop_front() {
        if visited >= MAX_NODES {
            // A bounded, incomplete walk cannot prove that an identity is
            // unique. Refuse refresh if any candidate remains uninspected.
            return Ok(None);
        }
        visited += 1;
        let accessible = proxy(connection, &key, ACCESSIBLE)?;
        // An element that vanished during the walk holds no identity.
        let Some(candidate) = optional(
            accessible.get_property::<String>("AccessibleId"),
            "read accessibility identity",
        )
        .or_gone()?
        else {
            continue;
        };
        if candidate.as_deref() == Some(accessible_id) {
            if matched.is_some() {
                return Ok(None);
            }
            matched = Some(key.clone());
        }
        let Some(children) = optional(
            accessible.call::<_, _, Vec<ObjectRefOwned>>("GetChildren", &()),
            "read accessibility children",
        )
        .or_gone()?
        else {
            continue;
        };
        queue.extend(children.unwrap_or_default().iter().filter_map(object_key));
    }
    Ok(matched)
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

fn focus_element(result: Result<bool, zbus::Error>) -> Result<(), ActionExecutionFailure> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: "AT-SPI refused to focus the accessibility element".into(),
            retryable: false,
        })),
        Err(error) if is_not_supported(&error) => {
            Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!("AT-SPI does not support focusing this element: {error}"),
                retryable: false,
            }))
        }
        Err(error) => Err(ActionExecutionFailure::indeterminate(
            ActionRoute::Accessibility,
            backend_failure(
                format!("could not focus accessibility element: {error}"),
                false,
            ),
        )),
    }
}

fn is_not_supported(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::Unsupported => true,
        zbus::Error::FDO(error) => matches!(error.as_ref(), zbus::fdo::Error::NotSupported(_)),
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.NotSupported"
        }
        _ => false,
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

/// The last read before acting on a resolved element: only an object that
/// has gone makes the target stale; any other failure is the backend's.
fn confirm_resolved<T>(result: Result<T, zbus::Error>) -> Result<(), ProtocolError> {
    match required(result, "confirm accessibility element before action") {
        Ok(_) => Ok(()),
        Err(ReadError::Gone) => Err(stale_target("accessibility element no longer resolves")),
        Err(ReadError::Failed(error)) => Err(error),
    }
}

/// The focus cache after AT-SPI has already moved focus, so a failure to
/// record it leaves the action's outcome indeterminate.
fn focus_cache<T>(lock: std::sync::LockResult<T>) -> Result<T, ActionExecutionFailure> {
    lock.map_err(|_| {
        ActionExecutionFailure::indeterminate(
            ActionRoute::Accessibility,
            backend_failure(
                "focus moved, but the accessibility focus cache is poisoned so later text entry cannot target it".to_owned(),
                false,
            ),
        )
    })
}

fn stale_target(message: &str) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::StaleAccessibilityTarget,
        message: message.into(),
        retryable: true,
    }
}

fn ambiguous_target(message: String) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::AmbiguousTarget,
        message,
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
    fn a_handle_whose_object_now_carries_another_identity_is_retired() {
        let mut registry = super::HandleRegistry::new();
        let key = ObjectKey {
            name: ":1.7".into(),
            path: "/org/a11y/atspi/accessible/42".into(),
        };
        let window = ObjectKey {
            name: ":1.7".into(),
            path: "/org/a11y/atspi/accessible/1".into(),
        };

        let first = registry.handle_for(key.clone(), window.clone(), Some("save".into()));
        let again = registry.handle_for(key.clone(), window.clone(), Some("save".into()));
        let unidentified = registry.handle_for(key.clone(), window.clone(), None);
        let replaced = registry.handle_for(key.clone(), window, Some("delete".into()));

        assert_eq!(again, first);
        assert_eq!(unidentified, first);
        assert_ne!(replaced, first);
        assert!(!registry.handles.contains_key(&first));
        assert_eq!(
            registry.handles[&replaced].accessible_id.as_deref(),
            Some("delete")
        );
    }

    #[test]
    fn state_names_outside_at_spi_are_refused() {
        assert!(super::require_known_state("focused").is_ok());
        assert!(super::require_known_state("multi-line").is_ok());
        assert_eq!(
            super::require_known_state("focussed").unwrap_err().code,
            waldo_protocol::ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn a_sole_window_contradicted_by_title_and_size_is_not_taken() {
        let frame = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 800.0,
            height: 600.0,
        };
        let other = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 300.0,
            height: 200.0,
        };

        let contradicted = select_window_root(
            vec![candidate("/2", "Preferences", other)],
            Some("Editor"),
            frame,
        )
        .unwrap();
        let unconfirmed = select_window_root(
            vec![candidate("/2", "Preferences", frame)],
            Some("Editor"),
            frame,
        )
        .unwrap();

        assert!(contradicted.is_none());
        assert!(unconfirmed.is_some());
    }

    #[test]
    fn a_sole_window_with_a_contradictory_title_is_not_taken_on_geometry_alone() {
        let frame = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 800.0,
            height: 600.0,
        };
        let unsized_window = || {
            let (key, mut node) = candidate("/2", "Preferences", frame);
            node.bounds = None;
            vec![(key, node)]
        };

        let contradicted = select_window_root(unsized_window(), Some("Editor"), frame).unwrap();
        let untitled_request = select_window_root(unsized_window(), None, frame).unwrap();

        assert!(contradicted.is_none());
        assert!(untitled_request.is_some());
    }

    #[test]
    fn only_a_gone_object_makes_the_final_resolution_read_stale() {
        let gone: Result<(), zbus::Error> = Err(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::UnknownObject("gone".into()),
        )));
        let failed: Result<(), zbus::Error> = Err(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::Timeout("application did not answer".into()),
        )));

        assert_eq!(
            super::confirm_resolved(gone).unwrap_err().code,
            waldo_protocol::ErrorCode::StaleAccessibilityTarget
        );
        let error = super::confirm_resolved(failed).unwrap_err();
        assert_eq!(error.code, waldo_protocol::ErrorCode::BackendFailure);
        assert!(
            error.message.contains("application did not answer"),
            "{}",
            error.message
        );
        assert!(super::confirm_resolved(Ok(())).is_ok());
    }

    #[test]
    fn a_poisoned_focus_cache_after_focus_moved_is_indeterminate() {
        let cache = std::sync::Mutex::new(());
        let _ = std::panic::catch_unwind(|| {
            let _guard = cache.lock().unwrap();
            panic!("poison the cache");
        });

        match super::focus_cache(cache.lock()) {
            Err(waldo_action::ActionExecutionFailure::Indeterminate { route, error }) => {
                assert_eq!(route, waldo_protocol::ActionRoute::Accessibility);
                assert_eq!(error.code, waldo_protocol::ErrorCode::BackendFailure);
            }
            Err(other) => panic!("focus already moved, so the failure is indeterminate: {other:?}"),
            Ok(_) => panic!("a poisoned cache must be reported"),
        }
    }

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
        .expect("window selection should not be ambiguous")
        .expect("title should identify one top-level window");

        assert_eq!(selected.0.path, "/second");
    }

    #[test]
    fn duplicate_window_titles_are_reported_as_ambiguous() {
        let bounds = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 300.0,
            height: 200.0,
        };
        let result = select_window_root(
            vec![
                candidate("/first", "Repeated", bounds),
                candidate("/second", "Repeated", bounds),
            ],
            Some("Repeated"),
            bounds,
        );
        let Err(error) = result else {
            panic!("duplicate titles must not select by similarity");
        };

        assert_eq!(error.code, waldo_protocol::ErrorCode::AmbiguousTarget);
        assert!(error.retryable);
    }

    fn candidate(path: &str, name: &str, bounds: LogicalRect) -> (ObjectKey, RawNode) {
        (
            ObjectKey {
                name: ":1.42".into(),
                path: path.into(),
            },
            RawNode {
                accessible_id: None,
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
