use std::{
    io::Write,
    io::{ErrorKind, Read},
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, mpsc::sync_channel},
    thread,
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};
use waldo_protocol::{ErrorCode, ProtocolError};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child,
    protocol::{
        wl_callback::{self, WlCallback},
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
    },
};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ROUNDTRIP_EVENTS: usize = 65_536;

pub(crate) fn probe() -> Result<String, ProtocolError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let mut queue = connection.new_event_queue::<OfferState>();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut state = OfferState::default();
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "discover Wayland globals",
    )?;
    let data_control = state.globals.iter().any(|global| {
        matches!(
            global.interface.as_str(),
            "ext_data_control_manager_v1" | "zwlr_data_control_manager_v1"
        )
    });
    if !data_control {
        return Err(unsupported(
            "stable clipboard preservation requires a Wayland seat and ext-data-control or wlr-data-control",
        ));
    }
    bind_seats(&registry, &qh, &mut state);
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "read Wayland seat names",
    )?;
    state
        .seats
        .iter()
        .find_map(|seat| seat.name.clone())
        .ok_or_else(|| unsupported("clipboard preservation requires a named Wayland seat"))
}

pub(crate) fn snapshot(
    seat_name: &str,
    max_bytes: usize,
) -> Result<Vec<(String, Vec<u8>)>, ProtocolError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let mut queue = connection.new_event_queue::<OfferState>();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut state = OfferState::default();
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "discover Wayland globals",
    )?;
    bind_seats(&registry, &qh, &mut state);
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "read Wayland seat names",
    )?;
    let seat = state
        .seats
        .iter()
        .find(|seat| seat.name.as_deref() == Some(seat_name))
        .map(|seat| seat.proxy.clone())
        .ok_or_else(|| {
            unsupported(format!(
                "configured clipboard seat {seat_name:?} is no longer available"
            ))
        })?;
    let ext_manager = state
        .globals
        .iter()
        .find(|global| global.interface == "ext_data_control_manager_v1")
        .cloned();
    let wlr_manager = state
        .globals
        .iter()
        .find(|global| global.interface == "zwlr_data_control_manager_v1")
        .cloned();
    match (ext_manager, wlr_manager) {
        (Some(global), _) => {
            let manager: ExtDataControlManagerV1 =
                registry.bind(global.name, global.version.min(1), &qh, ());
            manager.get_data_device(&seat, &qh, ());
        }
        (None, Some(global)) => {
            let manager: ZwlrDataControlManagerV1 =
                registry.bind(global.name, global.version.min(2), &qh, ());
            manager.get_data_device(&seat, &qh, ());
        }
        (None, None) => {
            return Err(unsupported(
                "stable clipboard preservation requires ext-data-control or wlr-data-control",
            ));
        }
    }
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "read current clipboard offer",
    )?;
    let Some(selection) = state.selection.clone() else {
        return Ok(Vec::new());
    };
    let generation = state.selection_generation;
    let mime_types = state
        .offers
        .iter()
        .find(|offer| offer.proxy.id() == selection.id())
        .map(|offer| offer.mime_types.clone())
        .ok_or_else(|| backend_failure("clipboard selection omitted its data offer", true))?;
    let mut representations = Vec::with_capacity(mime_types.len());
    let mut total = 0_usize;
    for mime_type in mime_types {
        let (reader, writer) = UnixStream::pair().map_err(|error| {
            backend_failure(format!("create clipboard transfer pipe: {error}"), true)
        })?;
        selection.receive(mime_type.clone(), writer.as_fd());
        connection.flush().map_err(|error| {
            backend_failure(format!("request clipboard representation: {error}"), true)
        })?;
        drop(writer);

        let remaining = max_bytes.saturating_sub(total);
        let data = read_representation(reader, remaining, deadline)?;
        total = total.saturating_add(data.len());
        if total > max_bytes {
            return Err(unsupported(format!(
                "clipboard representations exceed Waldo's {max_bytes}-byte restoration limit"
            )));
        }
        representations.push((mime_type, data));
    }

    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "confirm stable clipboard offer generation",
    )?;
    if state.selection_generation != generation
        || state.selection.as_ref().map(ClipboardOffer::id) != Some(selection.id())
    {
        return Err(backend_failure(
            "clipboard ownership changed while Waldo was preserving the offer",
            true,
        ));
    }

    Ok(representations)
}

pub(crate) fn publish(
    seat_name: &str,
    representations: Vec<(String, Vec<u8>)>,
) -> Result<Publication, ProtocolError> {
    let seat_name = seat_name.to_owned();
    let publication = Publication {
        failure: Arc::new(Mutex::new(None)),
    };
    let background_publication = publication.clone();
    coordinate_publication(
        SNAPSHOT_TIMEOUT,
        move |deadline| prepare_publication(&seat_name, representations, deadline),
        move |(mut queue, mut state)| {
            if let Err(error) = serve_owned_publication(&mut queue, &mut state) {
                background_publication.record_failure(error);
            }
        },
    )?;
    Ok(publication)
}

fn coordinate_publication<T, Prepare, Serve>(
    timeout: Duration,
    prepare: Prepare,
    serve: Serve,
) -> Result<(), ProtocolError>
where
    T: Send + 'static,
    Prepare: FnOnce(Instant) -> Result<T, ProtocolError> + Send + 'static,
    Serve: FnOnce(T) + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let (ready_tx, ready_rx) = sync_channel(1);
    let (accept_tx, accept_rx) = sync_channel(0);
    let worker = thread::spawn(move || match prepare(deadline) {
        Ok(prepared) => {
            if ready_tx.send(Ok(())).is_ok() && accept_rx.recv().is_ok() {
                serve(prepared);
            }
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    match ready_rx.recv_timeout(remaining) {
        Ok(Ok(())) => {
            if accept_tx.send(()).is_err() {
                worker.join().map_err(|_| {
                    backend_failure(
                        "prepare ordered clipboard publication: worker panicked",
                        true,
                    )
                })?;
                return Err(backend_failure(
                    "prepare ordered clipboard publication: worker exited before acceptance",
                    true,
                ));
            }
            drop(worker);
            Ok(())
        }
        Ok(Err(error)) => {
            drop(accept_tx);
            worker.join().map_err(|_| {
                backend_failure(
                    "prepare ordered clipboard publication: worker panicked",
                    true,
                )
            })?;
            Err(error)
        }
        Err(error) => {
            drop(accept_tx);
            worker.join().map_err(|_| {
                backend_failure(
                    "prepare ordered clipboard publication: worker panicked",
                    true,
                )
            })?;
            Err(backend_failure(
                format!("acknowledge ordered clipboard publication: {error}"),
                true,
            ))
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Publication {
    failure: Arc<Mutex<Option<ProtocolError>>>,
}

impl Publication {
    pub(crate) fn failure(&self) -> Option<ProtocolError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record_failure(&self, error: ProtocolError) {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }

    #[cfg(test)]
    pub(crate) fn failed(error: ProtocolError) -> Self {
        let publication = Self {
            failure: Arc::new(Mutex::new(None)),
        };
        publication.record_failure(error);
        publication
    }
}

fn serve_owned_publication(
    queue: &mut EventQueue<OfferState>,
    state: &mut OfferState,
) -> Result<(), ProtocolError> {
    while !state.publication_cancelled {
        let dispatched = queue.dispatch_pending(state).map_err(|error| {
            backend_failure(
                format!("serve ordered clipboard publication: dispatch events: {error}"),
                true,
            )
        })?;
        if let Some(error) = state.publication_failure.take() {
            return Err(error);
        }
        if state.publication_cancelled {
            break;
        }
        if dispatched > 0 {
            continue;
        }
        queue.flush().map_err(|error| {
            backend_failure(
                format!("serve ordered clipboard publication: flush requests: {error}"),
                true,
            )
        })?;
        let Some(read_guard) = queue.prepare_read() else {
            thread::sleep(Duration::from_millis(1));
            continue;
        };
        if wait_for_publication_readable(read_guard.connection_fd(), Duration::from_secs(1))? {
            read_guard.read().map_err(|error| {
                backend_failure(
                    format!("serve ordered clipboard publication: read events: {error}"),
                    true,
                )
            })?;
        }
    }
    Ok(())
}

fn wait_for_publication_readable(
    descriptor: std::os::fd::BorrowedFd<'_>,
    timeout: Duration,
) -> Result<bool, ProtocolError> {
    let mut descriptors = [PollFd::new(descriptor, PollFlags::POLLIN)];
    let timeout_ms = timeout.as_millis().clamp(1, u128::from(u16::MAX)) as u16;
    let ready = poll(&mut descriptors, timeout_ms).map_err(|error| {
        backend_failure(
            format!("serve ordered clipboard publication: wait for events: {error}"),
            true,
        )
    })?;
    if ready == 0 {
        return Ok(false);
    }
    let events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
    if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
        return Err(backend_failure(
            "serve ordered clipboard publication: Wayland connection became unavailable",
            true,
        ));
    }
    if !events.contains(PollFlags::POLLIN) {
        return Err(backend_failure(
            "serve ordered clipboard publication: poll returned without readable events",
            true,
        ));
    }
    Ok(true)
}

fn prepare_publication(
    seat_name: &str,
    representations: Vec<(String, Vec<u8>)>,
    deadline: Instant,
) -> Result<(EventQueue<OfferState>, OfferState), ProtocolError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| backend_failure(format!("connect to Wayland: {error}"), true))?;
    let mut queue = connection.new_event_queue::<OfferState>();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut state = OfferState {
        publication: representations,
        ..OfferState::default()
    };
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "discover Wayland globals for clipboard publication",
    )?;
    bind_seats(&registry, &qh, &mut state);
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "read Wayland seat names for clipboard publication",
    )?;
    let seat = state
        .seats
        .iter()
        .find(|seat| seat.name.as_deref() == Some(seat_name))
        .map(|seat| seat.proxy.clone())
        .ok_or_else(|| unsupported(format!("clipboard seat {seat_name:?} is unavailable")))?;
    let ext_manager = state
        .globals
        .iter()
        .find(|global| global.interface == "ext_data_control_manager_v1")
        .cloned();
    let wlr_manager = state
        .globals
        .iter()
        .find(|global| global.interface == "zwlr_data_control_manager_v1")
        .cloned();
    match (ext_manager, wlr_manager) {
        (Some(global), _) => {
            let manager: ExtDataControlManagerV1 =
                registry.bind(global.name, global.version.min(1), &qh, ());
            let source = manager.create_data_source(&qh, ());
            for (mime_type, _) in &state.publication {
                source.offer(mime_type.clone());
            }
            let device = manager.get_data_device(&seat, &qh, ());
            require_deadline(deadline, "claim ordered clipboard publication")?;
            device.set_selection(Some(&source));
            state.owned_source = Some(ClipboardSource::Ext { _proxy: source });
        }
        (None, Some(global)) => {
            let manager: ZwlrDataControlManagerV1 =
                registry.bind(global.name, global.version.min(2), &qh, ());
            let source = manager.create_data_source(&qh, ());
            for (mime_type, _) in &state.publication {
                source.offer(mime_type.clone());
            }
            let device = manager.get_data_device(&seat, &qh, ());
            require_deadline(deadline, "claim ordered clipboard publication")?;
            device.set_selection(Some(&source));
            state.owned_source = Some(ClipboardSource::Wlr { _proxy: source });
        }
        (None, None) => {
            return Err(unsupported(
                "ordered clipboard publication requires ext-data-control or wlr-data-control",
            ));
        }
    }
    bounded_roundtrip(
        &connection,
        &mut queue,
        &mut state,
        deadline,
        "acknowledge ordered clipboard publication",
    )?;
    Ok((queue, state))
}

fn require_deadline(deadline: Instant, operation: &str) -> Result<(), ProtocolError> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(backend_failure(
            format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
            true,
        ))
    }
}

#[derive(Default)]
struct OfferState {
    globals: Vec<GlobalRecord>,
    seats: Vec<SeatRecord>,
    offers: Vec<Offer>,
    selection: Option<ClipboardOffer>,
    selection_generation: u64,
    sync_done: bool,
    publication: Vec<(String, Vec<u8>)>,
    owned_source: Option<ClipboardSource>,
    publication_cancelled: bool,
    publication_failure: Option<ProtocolError>,
}

#[derive(Clone)]
struct GlobalRecord {
    name: u32,
    interface: String,
    version: u32,
}

struct SeatRecord {
    proxy: WlSeat,
    name: Option<String>,
}

struct Offer {
    proxy: ClipboardOffer,
    mime_types: Vec<String>,
}

#[derive(Clone)]
enum ClipboardOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

enum ClipboardSource {
    Ext { _proxy: ExtDataControlSourceV1 },
    Wlr { _proxy: ZwlrDataControlSourceV1 },
}

impl ClipboardOffer {
    fn id(&self) -> wayland_client::backend::ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    fn receive(&self, mime_type: String, descriptor: std::os::fd::BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime_type, descriptor),
            Self::Wlr(offer) => offer.receive(mime_type, descriptor),
        }
    }
}

impl Dispatch<WlRegistry, ()> for OfferState {
    fn event(
        state: &mut Self,
        _proxy: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => state.globals.push(GlobalRecord {
                name,
                interface,
                version,
            }),
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.retain(|global| global.name != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for OfferState {
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.sync_done = true;
        }
    }
}

impl Dispatch<WlSeat, ()> for OfferState {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_seat::Event::Name { name } = event
            && let Some(seat) = state
                .seats
                .iter_mut()
                .find(|seat| seat.proxy.id() == proxy.id())
        {
            seat.name = Some(name);
        }
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for OfferState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for OfferState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: <ZwlrDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => state.offers.push(Offer {
                proxy: ClipboardOffer::Wlr(id),
                mime_types: Vec::new(),
            }),
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection = id.map(ClipboardOffer::Wlr);
                state.selection_generation = state.selection_generation.saturating_add(1);
            }
            _ => {}
        }
    }

    event_created_child!(OfferState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ())
    ]);
}

impl Dispatch<ExtDataControlDeviceV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: <ExtDataControlDeviceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => state.offers.push(Offer {
                proxy: ClipboardOffer::Ext(id),
                mime_types: Vec::new(),
            }),
            ext_data_control_device_v1::Event::Selection { id } => {
                state.selection = id.map(ClipboardOffer::Ext);
                state.selection_generation = state.selection_generation.saturating_add(1);
            }
            _ => {}
        }
    }

    event_created_child!(OfferState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ())
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlOfferV1,
        event: <ZwlrDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(offer) = state
                .offers
                .iter_mut()
                .find(|offer| offer.proxy.id() == proxy.id())
        {
            offer.mime_types.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: <ExtDataControlOfferV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(offer) = state
                .offers
                .iter_mut()
                .find(|offer| offer.proxy.id() == proxy.id())
        {
            offer.mime_types.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrDataControlSourceV1,
        event: <ZwlrDataControlSourceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Err(error) = serve_publication(&state.publication, &mime_type, fd) {
                    state.publication_failure = Some(error);
                    state.publication_cancelled = true;
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                proxy.destroy();
                state.publication_cancelled = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for OfferState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: <ExtDataControlSourceV1 as Proxy>::Event,
        _data: &(),
        _connection: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Err(error) = serve_publication(&state.publication, &mime_type, fd) {
                    state.publication_failure = Some(error);
                    state.publication_cancelled = true;
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                proxy.destroy();
                state.publication_cancelled = true;
            }
            _ => {}
        }
    }
}

fn serve_publication(
    representations: &[(String, Vec<u8>)],
    mime_type: &str,
    fd: OwnedFd,
) -> Result<(), ProtocolError> {
    let (_, data) = representations
        .iter()
        .find(|(offered_mime, _)| offered_mime == mime_type)
        .ok_or_else(|| {
            backend_failure(
                format!("consumer requested unadvertised clipboard MIME type {mime_type:?}"),
                false,
            )
        })?;
    let mut destination = std::fs::File::from(fd);
    match destination.write_all(data) {
        Ok(()) => Ok(()),
        // Closing a per-request transfer descriptor abandons only that consumer's
        // read. The advertised offer and its ownership remain valid.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(backend_failure(
            format!("serve ordered clipboard representation to consumer: {error}"),
            true,
        )),
    }
}

fn bounded_roundtrip(
    connection: &Connection,
    queue: &mut EventQueue<OfferState>,
    state: &mut OfferState,
    deadline: Instant,
    operation: &str,
) -> Result<(), ProtocolError> {
    state.sync_done = false;
    connection.display().sync(&queue.handle(), ());
    let mut total_dispatched = 0_usize;
    while !state.sync_done {
        if Instant::now() >= deadline {
            return Err(backend_failure(
                format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
                true,
            ));
        }
        let dispatched = queue.dispatch_pending(state).map_err(|error| {
            backend_failure(
                format!("{operation}: dispatch Wayland events: {error}"),
                true,
            )
        })?;
        total_dispatched = total_dispatched.saturating_add(dispatched);
        if total_dispatched > MAX_ROUNDTRIP_EVENTS {
            return Err(backend_failure(
                format!("{operation} exceeded the bounded Wayland event budget"),
                true,
            ));
        }
        if state.sync_done {
            break;
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                backend_failure(
                    format!(
                        "{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                    ),
                    true,
                )
            })?;
        queue.flush().map_err(|error| {
            backend_failure(
                format!("{operation}: flush Wayland requests: {error}"),
                true,
            )
        })?;
        let Some(read_guard) = queue.prepare_read() else {
            if dispatched == 0 {
                thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            continue;
        };
        wait_for_readable(read_guard.connection_fd(), remaining, operation)?;
        read_guard.read().map_err(|error| {
            backend_failure(format!("{operation}: read Wayland events: {error}"), true)
        })?;
    }
    Ok(())
}

fn wait_for_readable(
    descriptor: std::os::fd::BorrowedFd<'_>,
    remaining: Duration,
    operation: &str,
) -> Result<(), ProtocolError> {
    let mut descriptors = [PollFd::new(descriptor, PollFlags::POLLIN)];
    let timeout_ms = remaining.as_millis().clamp(1, u128::from(u16::MAX)) as u16;
    let ready = poll(&mut descriptors, timeout_ms).map_err(|error| {
        backend_failure(
            format!("{operation}: wait for Wayland events: {error}"),
            true,
        )
    })?;
    if ready == 0 {
        return Err(backend_failure(
            format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
            true,
        ));
    }
    let events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
    if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
        return Err(backend_failure(
            format!("{operation}: Wayland connection became unavailable while waiting for events"),
            true,
        ));
    }
    if !events.contains(PollFlags::POLLIN) {
        return Err(backend_failure(
            format!("{operation}: Wayland poll returned without readable events"),
            true,
        ));
    }
    Ok(())
}

fn read_representation(
    mut reader: UnixStream,
    remaining_bytes: usize,
    deadline: Instant,
) -> Result<Vec<u8>, ProtocolError> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let remaining_time = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
            backend_failure(
                format!(
                    "receive clipboard representation exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                ),
                true,
            )
        })?;
        reader
            .set_read_timeout(Some(remaining_time))
            .map_err(|error| {
                backend_failure(format!("bound clipboard transfer time: {error}"), true)
            })?;
        let capacity = remaining_bytes.saturating_add(1).saturating_sub(data.len());
        let length = capacity.min(buffer.len());
        if length == 0 {
            return Ok(data);
        }
        match reader.read(&mut buffer[..length]) {
            Ok(0) => return Ok(data),
            Ok(read) => data.extend_from_slice(&buffer[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(backend_failure(
                    format!(
                        "receive clipboard representation exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                    ),
                    true,
                ));
            }
            Err(error) => {
                return Err(backend_failure(
                    format!("receive clipboard representation: {error}"),
                    true,
                ));
            }
        }
    }
}

fn unsupported(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: message.into(),
        retryable: false,
    }
}

fn bind_seats(registry: &WlRegistry, qh: &QueueHandle<OfferState>, state: &mut OfferState) {
    for global in state
        .globals
        .iter()
        .filter(|global| global.interface == "wl_seat")
    {
        let proxy: WlSeat = registry.bind(global.name, global.version.min(9), qh, ());
        state.seats.push(SeatRecord { proxy, name: None });
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
    use std::{
        io::Write as _,
        os::fd::AsFd as _,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{
        SNAPSHOT_TIMEOUT, coordinate_publication, read_representation, serve_publication,
        wait_for_publication_readable, wait_for_readable,
    };

    struct FakeOwnership(Arc<AtomicBool>);

    impl Drop for FakeOwnership {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    #[test]
    fn timeout_waits_until_a_late_worker_can_no_longer_retain_ownership() {
        let claimed = Arc::new(AtomicBool::new(false));
        let owned = Arc::new(AtomicBool::new(false));
        let served = Arc::new(AtomicBool::new(false));
        let prepare_claimed = Arc::clone(&claimed);
        let prepare_owned = Arc::clone(&owned);
        let serve_called = Arc::clone(&served);

        let error = coordinate_publication(
            Duration::from_millis(20),
            move |deadline| {
                thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(20),
                );
                prepare_claimed.store(true, Ordering::Release);
                prepare_owned.store(true, Ordering::Release);
                Ok(FakeOwnership(prepare_owned))
            },
            move |_ownership| serve_called.store(true, Ordering::Release),
        )
        .unwrap_err();

        assert!(
            error
                .message
                .contains("acknowledge ordered clipboard publication")
        );
        assert!(claimed.load(Ordering::Acquire));
        assert!(!owned.load(Ordering::Acquire));
        assert!(!served.load(Ordering::Acquire));
    }

    #[test]
    fn consumer_disconnect_does_not_invalidate_the_published_offer() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(reader);

        serve_publication(
            &[("text/plain".into(), b"payload".to_vec())],
            "text/plain",
            writer.into(),
        )
        .unwrap();
    }

    #[test]
    fn genuine_consumer_write_failure_is_payload_free_and_observable() {
        let destination = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();

        let error = serve_publication(
            &[("text/plain".into(), b"private-value".to_vec())],
            "text/plain",
            destination.into(),
        )
        .unwrap_err();

        assert!(
            error
                .message
                .contains("serve ordered clipboard representation")
        );
        assert!(!error.message.contains("private-value"));
    }

    #[test]
    fn publication_readiness_waits_when_idle_and_fails_closed_on_hangup() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = Instant::now();
        assert!(
            !wait_for_publication_readable(reader.as_fd(), std::time::Duration::from_millis(20),)
                .unwrap()
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(10));

        writer.write_all(b"x").unwrap();
        assert!(
            wait_for_publication_readable(reader.as_fd(), std::time::Duration::from_secs(1),)
                .unwrap()
        );

        drop(writer);
        let error =
            wait_for_publication_readable(reader.as_fd(), std::time::Duration::from_secs(1))
                .unwrap_err();
        assert!(error.message.contains("became unavailable"));
    }

    #[test]
    fn hangup_readiness_fails_immediately_instead_of_spinning() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(writer);
        let started = Instant::now();
        let error = wait_for_readable(
            reader.as_fd(),
            std::time::Duration::from_secs(1),
            "test Wayland roundtrip",
        )
        .unwrap_err();

        assert!(error.message.contains("became unavailable"));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn a_stream_that_never_finishes_is_bounded_by_the_snapshot_deadline() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            let block = [b'x'; 4096];
            while writer.write_all(&block).is_ok() {
                thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        let started = Instant::now();
        let error = read_representation(
            reader,
            usize::MAX,
            started + std::time::Duration::from_millis(40),
        )
        .unwrap_err();

        assert!(error.message.contains("snapshot limit"));
        assert!(started.elapsed() < SNAPSHOT_TIMEOUT);
        writer.join().unwrap();
    }
}
