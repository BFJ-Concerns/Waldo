//! The single controller authority shared by Waldo's MCP and CLI bindings.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use waldo_protocol::{
    ClientId, ConnectionId, DrivingSessionId, LogicalRect, ObservationId, OutputId,
    OutputTransform, SourceSelector, WindowId,
};

const MAX_RETAINED_OBSERVATIONS: usize = 256;

/// Desktop facts whose stability determines whether a planned action is safe.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservationGrounding {
    pub source: SourceSelector,
    pub output_id: OutputId,
    pub output_logical_bounds: LogicalRect,
    pub observed_logical_bounds: LogicalRect,
    pub scale: f64,
    pub transform: OutputTransform,
    pub focused_window: Option<WindowId>,
    /// Whether the observation requested an AT-SPI semantic overlay.
    pub include_accessibility: bool,
    pub cursor_position: Option<waldo_protocol::LogicalPoint>,
    /// Process-local fingerprint used only for exact frame-change predicates.
    pub frame_fingerprint: u64,
    /// Cursor-free fingerprint used for application-state change predicates.
    pub application_frame_fingerprint: u64,
}

/// Cooperative cancellation shared with one currently executing action batch.
#[derive(Clone, Debug)]
pub struct ExecutionToken {
    cancelled: Arc<AtomicBool>,
}

impl ExecutionToken {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Authority reserved atomically for one action batch.
#[derive(Clone, Debug)]
pub struct ExecutionAuthority {
    session: DrivingSession,
    cancellation: ExecutionToken,
    acquired_lease: bool,
}

impl ExecutionAuthority {
    pub const fn session(&self) -> DrivingSession {
        self.session
    }

    pub fn cancellation(&self) -> &ExecutionToken {
        &self.cancellation
    }

    pub const fn acquired_lease(&self) -> bool {
        self.acquired_lease
    }
}

#[derive(Clone, Debug, PartialEq)]
struct RetainedObservation {
    owner: ConnectionId,
    grounding: ObservationGrounding,
}

/// The controller-owned state for one period of foreground driving authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrivingSession {
    id: DrivingSessionId,
    owner: ConnectionId,
    last_activity: Duration,
}

impl DrivingSession {
    /// Returns the identity of this non-resumable driving session.
    pub const fn id(self) -> DrivingSessionId {
        self.id
    }

    /// Returns the connection that currently owns the driving lease.
    pub const fn owner(self) -> ConnectionId {
        self.owner
    }
}

/// A controller connection or driving-lease operation was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseError {
    /// The supplied connection is no longer attached to this controller.
    UnknownConnection(ConnectionId),
    /// Another connection currently owns the exclusive driving lease.
    HeldBy(ConnectionId),
    /// No connection currently owns the driving lease.
    NoActiveLease,
    /// The operation came from a connected client that does not own the lease.
    NotOwner { owner: ConnectionId },
    /// The owning connection already has a batch in flight.
    ExecutionInProgress,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownConnection(connection) => {
                write!(
                    formatter,
                    "unknown controller connection {}",
                    connection.get()
                )
            }
            Self::HeldBy(owner) => {
                write!(
                    formatter,
                    "the driving lease is held by connection {}",
                    owner.get()
                )
            }
            Self::NoActiveLease => formatter.write_str("there is no active driving lease"),
            Self::NotOwner { owner } => {
                write!(
                    formatter,
                    "the driving lease is owned by connection {}",
                    owner.get()
                )
            }
            Self::ExecutionInProgress => {
                formatter.write_str("the driving connection already has an action batch in flight")
            }
        }
    }
}

impl Error for LeaseError {}

/// The session-scoped authority shared by every Waldo binding.
///
/// Read-only clients need only remain connected. Mutation first acquires the
/// single driving lease, which is bound to the specific connection rather than
/// the logical client identity.
#[derive(Debug)]
pub struct Controller {
    idle_timeout: Duration,
    next_connection: u64,
    next_driving_session: u64,
    next_observation: u64,
    connections: BTreeMap<ConnectionId, ClientId>,
    driving_session: Option<DrivingSession>,
    active_execution: Option<(ConnectionId, ExecutionToken)>,
    observations: BTreeMap<ObservationId, RetainedObservation>,
}

impl Controller {
    /// Creates a controller with the configured driving-lease idle timeout.
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            next_connection: 1,
            next_driving_session: 1,
            next_observation: 1,
            connections: BTreeMap::new(),
            driving_session: None,
            active_execution: None,
            observations: BTreeMap::new(),
        }
    }

    /// Attaches a logical client and returns a fresh connection identity.
    pub fn connect(&mut self, client: ClientId) -> ConnectionId {
        let connection = ConnectionId::new(self.take_connection_sequence());
        self.connections.insert(connection, client);
        connection
    }

    /// Reports whether the connection is still attached to this controller.
    pub fn is_connected(&self, connection: ConnectionId) -> bool {
        self.connections.contains_key(&connection)
    }

    /// Returns the logical identity supplied by a connected client.
    pub fn client(&self, connection: ConnectionId) -> Option<&ClientId> {
        self.connections.get(&connection)
    }

    /// Disconnects a client and releases any driving authority bound to it.
    pub fn disconnect(&mut self, connection: ConnectionId) -> Result<(), LeaseError> {
        if self.connections.remove(&connection).is_none() {
            return Err(LeaseError::UnknownConnection(connection));
        }

        if self
            .driving_session
            .is_some_and(|lease| lease.owner == connection)
        {
            self.cancel_active_execution();
            self.driving_session = None;
        }
        Ok(())
    }

    /// Returns the current driving session for read-only inspection.
    pub const fn driving_session(&self) -> Option<DrivingSession> {
        self.driving_session
    }

    /// Records the safety-relevant facts behind a fresh observation identity.
    pub fn record_observation(
        &mut self,
        owner: ConnectionId,
        grounding: ObservationGrounding,
    ) -> ObservationId {
        let sequence = self.next_observation;
        self.next_observation = self
            .next_observation
            .checked_add(1)
            .expect("observation identifier space exhausted");
        let id = ObservationId::new(sequence);
        self.observations
            .insert(id, RetainedObservation { owner, grounding });
        if self.observations.len() > MAX_RETAINED_OBSERVATIONS {
            let oldest = *self
                .observations
                .first_key_value()
                .expect("an over-capacity observation ledger is non-empty")
                .0;
            self.observations.remove(&oldest);
        }
        id
    }

    /// Returns the facts originally attached to a retained observation.
    pub fn observation(
        &self,
        owner: ConnectionId,
        id: ObservationId,
    ) -> Option<&ObservationGrounding> {
        self.observations
            .get(&id)
            .filter(|observation| observation.owner == owner)
            .map(|observation| &observation.grounding)
    }

    /// Atomically acquires or refreshes the lease for a mutating operation.
    pub fn acquire_driving_lease(
        &mut self,
        connection: ConnectionId,
        now: Duration,
    ) -> Result<DrivingSession, LeaseError> {
        self.require_connection(connection)?;

        if let Some(session) = &mut self.driving_session {
            if session.owner != connection {
                return Err(LeaseError::HeldBy(session.owner));
            }

            // A subsequent action from the current driver is activity, not a
            // second lease acquisition. Keep the same session identity.
            session.last_activity = now;
            return Ok(*session);
        }

        let session = DrivingSession {
            id: DrivingSessionId::new(self.take_driving_session_sequence()),
            owner: connection,
            last_activity: now,
        };
        self.driving_session = Some(session);
        Ok(session)
    }

    /// Atomically acquires authority and marks one cancellable batch in flight.
    pub fn begin_execution(
        &mut self,
        connection: ConnectionId,
        now: Duration,
    ) -> Result<ExecutionAuthority, LeaseError> {
        self.require_connection(connection)?;
        if self.active_execution.is_some() {
            return Err(LeaseError::ExecutionInProgress);
        }
        let acquired_lease = self.driving_session.is_none();
        let session = self.acquire_driving_lease(connection, now)?;
        let token = ExecutionToken::new();
        self.active_execution = Some((connection, token.clone()));
        Ok(ExecutionAuthority {
            session,
            cancellation: token,
            acquired_lease,
        })
    }

    /// Clears the in-flight marker after the batch has stopped touching backends.
    pub fn finish_execution(&mut self, connection: ConnectionId) {
        if self
            .active_execution
            .as_ref()
            .is_some_and(|(owner, _)| *owner == connection)
        {
            self.active_execution = None;
        }
    }

    pub fn execution_in_progress(&self, connection: ConnectionId) -> bool {
        self.active_execution
            .as_ref()
            .is_some_and(|(owner, _)| *owner == connection)
    }

    /// Rolls back provisional authority when pre-dispatch validation fails.
    pub fn abort_execution(&mut self, connection: ConnectionId, release_lease: bool) {
        if self
            .active_execution
            .as_ref()
            .is_some_and(|(owner, _)| *owner == connection)
        {
            self.cancel_active_execution();
            self.active_execution = None;
            if release_lease
                && self
                    .driving_session
                    .is_some_and(|session| session.owner == connection)
            {
                self.driving_session = None;
            }
        }
    }

    /// Revokes all session authority before the controller service terminates.
    pub fn terminate(&mut self) {
        self.cancel_active_execution();
        self.driving_session = None;
        self.connections.clear();
    }

    /// Releases the driving lease when its idle timeout has elapsed.
    ///
    /// The caller supplies time from one monotonic clock. A backwards value is
    /// treated as no elapsed time rather than expiring authority spuriously.
    pub fn expire_idle_lease(&mut self, now: Duration) -> bool {
        // A batch actively touching backends is not idle. Its cancellation
        // boundary is Stop/disconnect; the lease becomes expirable once the
        // batch clears its in-flight marker.
        if self.active_execution.is_some() {
            return false;
        }
        let Some(session) = self.driving_session else {
            return false;
        };

        if now.saturating_sub(session.last_activity) < self.idle_timeout {
            return false;
        }

        self.cancel_active_execution();
        self.driving_session = None;
        true
    }

    /// Stops the active driving session when requested by its owner.
    pub fn stop(&mut self, connection: ConnectionId) -> Result<(), LeaseError> {
        self.require_connection(connection)?;
        let session = self.driving_session.ok_or(LeaseError::NoActiveLease)?;
        if session.owner != connection {
            return Err(LeaseError::NotOwner {
                owner: session.owner,
            });
        }

        self.cancel_active_execution();
        self.driving_session = None;
        Ok(())
    }

    /// Explicitly transfers driving authority to another connected client.
    ///
    /// Handover starts a new session. The previous session identity must never
    /// authorise later actions, even when the target has the same logical
    /// [`ClientId`].
    pub fn handover(
        &mut self,
        owner: ConnectionId,
        target: ConnectionId,
        now: Duration,
    ) -> Result<DrivingSession, LeaseError> {
        self.require_connection(owner)?;
        self.require_connection(target)?;

        let current = self.driving_session.ok_or(LeaseError::NoActiveLease)?;
        if current.owner != owner {
            return Err(LeaseError::NotOwner {
                owner: current.owner,
            });
        }

        self.cancel_active_execution();
        let transferred = DrivingSession {
            id: DrivingSessionId::new(self.take_driving_session_sequence()),
            owner: target,
            last_activity: now,
        };
        self.driving_session = Some(transferred);
        Ok(transferred)
    }

    fn require_connection(&self, connection: ConnectionId) -> Result<(), LeaseError> {
        if self.is_connected(connection) {
            Ok(())
        } else {
            Err(LeaseError::UnknownConnection(connection))
        }
    }

    fn cancel_active_execution(&mut self) {
        if let Some((_, token)) = &self.active_execution {
            token.cancel();
        }
    }

    fn take_connection_sequence(&mut self) -> u64 {
        let sequence = self.next_connection;
        self.next_connection = self
            .next_connection
            .checked_add(1)
            .expect("controller connection identifier space exhausted");
        sequence
    }

    fn take_driving_session_sequence(&mut self) -> u64 {
        let sequence = self.next_driving_session;
        self.next_driving_session = self
            .next_driving_session
            .checked_add(1)
            .expect("driving session identifier space exhausted");
        sequence
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use waldo_protocol::{ClientId, ConnectionId};

    use super::{Controller, LeaseError, MAX_RETAINED_OBSERVATIONS, ObservationGrounding};

    const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

    fn grounding(output: &str) -> ObservationGrounding {
        ObservationGrounding {
            source: waldo_protocol::SourceSelector::Output {
                output_id: waldo_protocol::OutputId(output.into()),
            },
            output_id: waldo_protocol::OutputId(output.into()),
            output_logical_bounds: waldo_protocol::LogicalRect {
                origin: waldo_protocol::LogicalPoint { x: 0.0, y: 0.0 },
                width: 1920.0,
                height: 1080.0,
            },
            observed_logical_bounds: waldo_protocol::LogicalRect {
                origin: waldo_protocol::LogicalPoint { x: 0.0, y: 0.0 },
                width: 1920.0,
                height: 1080.0,
            },
            scale: 1.0,
            transform: waldo_protocol::OutputTransform::Normal,
            focused_window: None,
            include_accessibility: false,
            cursor_position: None,
            frame_fingerprint: 0,
            application_frame_fingerprint: 0,
        }
    }

    fn client(name: &str) -> ClientId {
        ClientId::new(name).expect("test client identifiers are non-empty")
    }

    #[test]
    fn read_only_connections_coexist_without_a_driving_lease() {
        let mut controller = Controller::new(IDLE_TIMEOUT);

        let first = controller.connect(client("mcp"));
        let second = controller.connect(client("cli"));

        assert!(controller.is_connected(first));
        assert!(controller.is_connected(second));
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn first_mutating_connection_acquires_the_exclusive_lease() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        let cli = controller.connect(client("cli"));

        let session = controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .expect("the first driver should acquire the lease");

        assert_eq!(session.owner(), mcp);
        assert_eq!(controller.driving_session(), Some(session));
        assert_eq!(
            controller.acquire_driving_lease(cli, Duration::ZERO),
            Err(LeaseError::HeldBy(mcp))
        );
    }

    #[test]
    fn disconnect_releases_the_driving_lease() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .unwrap();

        controller.disconnect(mcp).unwrap();

        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn reconnecting_the_same_client_does_not_restore_its_lease() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let original = controller.connect(client("mcp"));
        controller
            .acquire_driving_lease(original, Duration::ZERO)
            .unwrap();
        controller.disconnect(original).unwrap();

        let replacement = controller.connect(client("mcp"));

        assert_ne!(replacement, original);
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn idle_expiry_releases_the_lease_at_the_timeout_boundary() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        controller
            .acquire_driving_lease(mcp, Duration::from_secs(5))
            .unwrap();

        assert!(!controller.expire_idle_lease(Duration::from_secs(34)));
        assert!(controller.expire_idle_lease(Duration::from_secs(35)));
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn only_the_owner_can_stop_the_driving_session() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        let observer = controller.connect(client("cli"));
        controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .unwrap();

        assert_eq!(
            controller.stop(observer),
            Err(LeaseError::NotOwner { owner: mcp })
        );
        assert_eq!(controller.driving_session().unwrap().owner(), mcp);
        assert!(controller.stop(mcp).is_ok());
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn explicit_handover_transfers_the_lease_to_a_connected_client() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        let cli = controller.connect(client("cli"));
        let original = controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .unwrap();

        let transferred = controller
            .handover(mcp, cli, Duration::from_secs(1))
            .unwrap();

        assert_ne!(transferred.id(), original.id());
        assert_eq!(transferred.owner(), cli);
        assert_eq!(controller.driving_session(), Some(transferred));
    }

    #[test]
    fn repeated_actions_refresh_the_existing_session() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        let original = controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .unwrap();

        let refreshed = controller
            .acquire_driving_lease(mcp, Duration::from_secs(20))
            .unwrap();

        assert_eq!(refreshed.id(), original.id());
        assert!(!controller.expire_idle_lease(Duration::from_secs(49)));
        assert!(controller.expire_idle_lease(Duration::from_secs(50)));
    }

    #[test]
    fn stop_cancels_the_owners_in_flight_execution() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("driver"));
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();
        let token = authority.cancellation().clone();

        controller.stop(owner).unwrap();

        assert!(token.is_cancelled());
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn cancelled_execution_must_quiesce_before_another_batch_begins() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("driver"));
        let next = controller.connect(client("next"));
        controller.begin_execution(owner, Duration::ZERO).unwrap();
        controller.stop(owner).unwrap();

        assert!(matches!(
            controller.begin_execution(next, Duration::from_secs(1)),
            Err(LeaseError::ExecutionInProgress)
        ));

        controller.finish_execution(owner);
        assert!(
            controller
                .begin_execution(next, Duration::from_secs(2))
                .is_ok()
        );
    }

    #[test]
    fn termination_cancels_execution_and_forgets_all_connections() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("driver"));
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();
        let token = authority.cancellation().clone();

        controller.terminate();

        assert!(token.is_cancelled());
        assert!(!controller.is_connected(owner));
        assert_eq!(controller.driving_session(), None);
    }

    #[test]
    fn a_second_batch_cannot_interleave_with_the_first() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("driver"));
        controller.begin_execution(owner, Duration::ZERO).unwrap();

        assert!(matches!(
            controller.begin_execution(owner, Duration::from_secs(1)),
            Err(LeaseError::ExecutionInProgress)
        ));
    }

    #[test]
    fn failed_validation_rolls_back_only_a_newly_acquired_lease() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("driver"));
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();
        assert!(authority.acquired_lease());

        controller.abort_execution(owner, authority.acquired_lease());
        assert_eq!(controller.driving_session(), None);

        controller
            .acquire_driving_lease(owner, Duration::from_secs(1))
            .unwrap();
        let authority = controller
            .begin_execution(owner, Duration::from_secs(2))
            .unwrap();
        assert!(!authority.acquired_lease());
        controller.abort_execution(owner, authority.acquired_lease());
        assert_eq!(controller.driving_session().unwrap().owner(), owner);
    }

    #[test]
    fn disconnected_clients_cannot_mutate_or_stop() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let missing = ConnectionId::new(99);

        assert_eq!(
            controller.acquire_driving_lease(missing, Duration::ZERO),
            Err(LeaseError::UnknownConnection(missing))
        );
        assert_eq!(
            controller.stop(missing),
            Err(LeaseError::UnknownConnection(missing))
        );
        assert_eq!(
            controller.disconnect(missing),
            Err(LeaseError::UnknownConnection(missing))
        );
    }

    #[test]
    fn handover_requires_an_owner_an_active_lease_and_a_connected_target() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let mcp = controller.connect(client("mcp"));
        let cli = controller.connect(client("cli"));
        let missing = ConnectionId::new(99);

        assert_eq!(
            controller.handover(mcp, cli, Duration::ZERO),
            Err(LeaseError::NoActiveLease)
        );
        controller
            .acquire_driving_lease(mcp, Duration::ZERO)
            .unwrap();
        assert_eq!(
            controller.handover(cli, mcp, Duration::ZERO),
            Err(LeaseError::NotOwner { owner: mcp })
        );
        assert_eq!(
            controller.handover(mcp, missing, Duration::ZERO),
            Err(LeaseError::UnknownConnection(missing))
        );
    }

    #[test]
    fn observation_identifiers_are_unique_within_the_controller_session() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("observer"));

        let first = controller.record_observation(owner, grounding("DP-1"));
        let second = controller.record_observation(owner, grounding("DP-2"));

        assert_ne!(first, second);
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(
            controller.observation(owner, first),
            Some(&grounding("DP-1"))
        );
        assert_eq!(
            controller.observation(owner, second),
            Some(&grounding("DP-2"))
        );
    }

    #[test]
    fn another_connection_cannot_authorise_actions_from_a_guessed_observation_id() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let observer = controller.connect(client("observer"));
        let other = controller.connect(client("other"));
        let id = controller.record_observation(observer, grounding("DP-1"));

        assert_eq!(controller.observation(other, id), None);
        assert_eq!(
            controller.observation(observer, id),
            Some(&grounding("DP-1"))
        );
    }

    #[test]
    fn observation_ledger_evicts_oldest_grounding_at_its_fixed_bound() {
        let mut controller = Controller::new(IDLE_TIMEOUT);
        let owner = controller.connect(client("observer"));
        let oldest = controller.record_observation(owner, grounding("oldest"));
        for sequence in 1..=MAX_RETAINED_OBSERVATIONS {
            controller.record_observation(owner, grounding(&format!("output-{sequence}")));
        }

        assert_eq!(controller.observation(owner, oldest), None);
        assert_eq!(controller.observations.len(), MAX_RETAINED_OBSERVATIONS);
    }
}
