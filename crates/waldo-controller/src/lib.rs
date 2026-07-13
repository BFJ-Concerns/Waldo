//! The single controller authority shared by Waldo's MCP and CLI bindings.

use std::{collections::BTreeMap, error::Error, fmt, time::Duration};

use waldo_protocol::{ClientId, ConnectionId, DrivingSessionId};

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
    connections: BTreeMap<ConnectionId, ClientId>,
    driving_session: Option<DrivingSession>,
}

impl Controller {
    /// Creates a controller with the configured driving-lease idle timeout.
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            idle_timeout,
            next_connection: 1,
            next_driving_session: 1,
            connections: BTreeMap::new(),
            driving_session: None,
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
            self.driving_session = None;
        }
        Ok(())
    }

    /// Returns the current driving session for read-only inspection.
    pub const fn driving_session(&self) -> Option<DrivingSession> {
        self.driving_session
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

    /// Releases the driving lease when its idle timeout has elapsed.
    ///
    /// The caller supplies time from one monotonic clock. A backwards value is
    /// treated as no elapsed time rather than expiring authority spuriously.
    pub fn expire_idle_lease(&mut self, now: Duration) -> bool {
        let Some(session) = self.driving_session else {
            return false;
        };

        if now.saturating_sub(session.last_activity) < self.idle_timeout {
            return false;
        }

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

    use super::{Controller, LeaseError};

    const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
}
