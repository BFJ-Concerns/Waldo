use std::{
    io::Write,
    io::{ErrorKind, Read},
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::sync_channel,
    },
    thread,
    time::{Duration, Instant},
};

use nix::poll::{PollFd, PollFlags, poll};
use waldo_protocol::ProtocolError;
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

/// A Wayland connection whose globals and named seats have been read.
struct Discovered {
    connection: Connection,
    queue: EventQueue<OfferState>,
    registry: WlRegistry,
    state: OfferState,
}

/// Connects, then reads the globals and every seat's name within the
/// deadline: the setup every clipboard operation starts from.
fn discover(
    state: OfferState,
    deadline: Instant,
    purpose: &str,
) -> Result<Discovered, ProtocolError> {
    let connection = Connection::connect_to_env().map_err(|error| {
        ProtocolError::backend_failure(format!("connect to Wayland: {error}"), true)
    })?;
    let queue = connection.new_event_queue::<OfferState>();
    let qh = queue.handle();
    let registry = connection.display().get_registry(&qh, ());
    let mut discovered = Discovered {
        connection,
        queue,
        registry,
        state,
    };
    discovered.roundtrip(deadline, &format!("discover Wayland globals {purpose}"))?;
    bind_seats(&discovered.registry, &qh, &mut discovered.state);
    discovered.roundtrip(deadline, &format!("read Wayland seat names {purpose}"))?;
    Ok(discovered)
}

/// The data-control manager the compositor offers, preferring the
/// standard ext protocol over the wlr one.
enum DataControlManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

impl Discovered {
    fn roundtrip(&mut self, deadline: Instant, operation: &str) -> Result<(), ProtocolError> {
        bounded_roundtrip(
            &self.connection,
            &mut self.queue,
            &mut self.state,
            deadline,
            operation,
        )
    }

    fn has_data_control(&self) -> bool {
        self.state.globals.iter().any(|global| {
            matches!(
                global.interface.as_str(),
                "ext_data_control_manager_v1" | "zwlr_data_control_manager_v1"
            )
        })
    }

    fn seat(&self, seat_name: &str) -> Option<WlSeat> {
        self.state
            .seats
            .iter()
            .find(|seat| seat.name.as_deref() == Some(seat_name))
            .map(|seat| seat.proxy.clone())
    }

    fn data_control_manager(&self) -> Option<DataControlManager> {
        let qh = self.queue.handle();
        let global = |interface: &str| {
            self.state
                .globals
                .iter()
                .find(|global| global.interface == interface)
                .cloned()
        };
        if let Some(global) = global("ext_data_control_manager_v1") {
            return Some(DataControlManager::Ext(self.registry.bind(
                global.name,
                global.version.min(1),
                &qh,
                (),
            )));
        }
        global("zwlr_data_control_manager_v1").map(|global| {
            DataControlManager::Wlr(
                self.registry
                    .bind(global.name, global.version.min(2), &qh, ()),
            )
        })
    }

    /// Fails when the compositor ended Waldo's data-control device, which
    /// no longer reports the clipboard after that.
    fn require_live_device(&self, operation: &str) -> Result<(), ProtocolError> {
        if self.state.device_finished {
            return Err(ProtocolError::backend_failure(
                format!("{operation}: the compositor ended Waldo's clipboard data control"),
                true,
            ));
        }
        Ok(())
    }
}

pub(crate) fn probe() -> Result<String, ProtocolError> {
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let discovered = discover(OfferState::default(), deadline, "for the clipboard")?;
    if !discovered.has_data_control() {
        return Err(ProtocolError::unsupported_capability(
            "stable clipboard preservation requires a Wayland seat and ext-data-control or wlr-data-control",
        ));
    }
    discovered
        .state
        .seats
        .iter()
        .find_map(|seat| seat.name.clone())
        .ok_or_else(|| {
            ProtocolError::unsupported_capability(
                "clipboard preservation requires a named Wayland seat",
            )
        })
}

pub(crate) fn snapshot(
    seat_name: &str,
    max_bytes: usize,
) -> Result<Vec<(String, Vec<u8>)>, ProtocolError> {
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let mut discovered = discover(OfferState::default(), deadline, "for the clipboard")?;
    let seat = discovered.seat(seat_name).ok_or_else(|| {
        ProtocolError::unsupported_capability(format!(
            "configured clipboard seat {seat_name:?} is no longer available"
        ))
    })?;
    let qh = discovered.queue.handle();
    match discovered.data_control_manager() {
        Some(DataControlManager::Ext(manager)) => {
            manager.get_data_device(&seat, &qh, ());
        }
        Some(DataControlManager::Wlr(manager)) => {
            manager.get_data_device(&seat, &qh, ());
        }
        None => {
            return Err(ProtocolError::unsupported_capability(
                "stable clipboard preservation requires ext-data-control or wlr-data-control",
            ));
        }
    }
    discovered.roundtrip(deadline, "read current clipboard offer")?;
    // A device the compositor ended reports no selection, which would
    // otherwise read as an empty clipboard.
    discovered.require_live_device("read current clipboard offer")?;
    let Discovered {
        connection,
        mut queue,
        mut state,
        ..
    } = discovered;
    let Some(selection) = state.selection.clone() else {
        return Ok(Vec::new());
    };
    let generation = state.selection_generation;
    let mime_types = state
        .offers
        .iter()
        .find(|offer| offer.proxy.id() == selection.id())
        .map(|offer| offer.mime_types.clone())
        .ok_or_else(|| {
            ProtocolError::backend_failure("clipboard selection omitted its data offer", true)
        })?;
    let mut representations = Vec::with_capacity(mime_types.len());
    let mut total = 0_usize;
    for mime_type in mime_types {
        let (reader, writer) = UnixStream::pair().map_err(|error| {
            ProtocolError::backend_failure(format!("create clipboard transfer pipe: {error}"), true)
        })?;
        selection.receive(mime_type.clone(), writer.as_fd());
        connection.flush().map_err(|error| {
            ProtocolError::backend_failure(
                format!("request clipboard representation: {error}"),
                true,
            )
        })?;
        drop(writer);

        let remaining = max_bytes.saturating_sub(total);
        let data = read_representation(reader, remaining, deadline)?;
        total = total.saturating_add(data.len());
        if total > max_bytes {
            return Err(ProtocolError::unsupported_capability(format!(
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
    if state.device_finished {
        return Err(ProtocolError::backend_failure(
            "confirm stable clipboard offer generation: the compositor ended Waldo's clipboard data control",
            true,
        ));
    }
    if state.selection_generation != generation
        || state.selection.as_ref().map(ClipboardOffer::id) != Some(selection.id())
    {
        return Err(ProtocolError::backend_failure(
            "clipboard ownership changed while Waldo was preserving the offer",
            true,
        ));
    }

    Ok(representations)
}

/// Owns the clipboard with these representations, served until another
/// owner replaces them.
pub(crate) fn publish(
    seat_name: &str,
    representations: Vec<(String, Vec<u8>)>,
) -> Result<Publication, ProtocolError> {
    publish_with(seat_name, representations, None)
}

/// Owns the clipboard for exactly one completed transfer, then withdraws
/// the offer; [`Publication::delivered`] reports that transfer.
pub(crate) fn publish_once(
    seat_name: &str,
    representations: Vec<(String, Vec<u8>)>,
) -> Result<Publication, ProtocolError> {
    publish_with(seat_name, representations, Some(1))
}

fn publish_with(
    seat_name: &str,
    representations: Vec<(String, Vec<u8>)>,
    transfers: Option<u32>,
) -> Result<Publication, ProtocolError> {
    let seat_name = seat_name.to_owned();
    let publication = Publication {
        failure: Arc::new(Mutex::new(None)),
        delivered: Arc::new(AtomicBool::new(false)),
    };
    let background_publication = publication.clone();
    let delivered = Arc::clone(&publication.delivered);
    coordinate_publication(
        SNAPSHOT_TIMEOUT,
        move |deadline| {
            prepare_publication(
                &seat_name,
                OfferState {
                    publication: representations,
                    transfers_remaining: transfers,
                    delivered: Some(delivered),
                    ..OfferState::default()
                },
                deadline,
            )
        },
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
                    ProtocolError::backend_failure(
                        "prepare ordered clipboard publication: worker panicked",
                        true,
                    )
                })?;
                return Err(ProtocolError::backend_failure(
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
                ProtocolError::backend_failure(
                    "prepare ordered clipboard publication: worker panicked",
                    true,
                )
            })?;
            Err(error)
        }
        Err(error) => {
            drop(accept_tx);
            worker.join().map_err(|_| {
                ProtocolError::backend_failure(
                    "prepare ordered clipboard publication: worker panicked",
                    true,
                )
            })?;
            Err(ProtocolError::backend_failure(
                format!("acknowledge ordered clipboard publication: {error}"),
                true,
            ))
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Publication {
    failure: Arc<Mutex<Option<ProtocolError>>>,
    delivered: Arc<AtomicBool>,
}

impl Publication {
    /// Whether a one-shot offer completed its transfer to a consumer.
    pub(crate) fn delivered(&self) -> bool {
        self.delivered.load(Ordering::Acquire)
    }

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
            delivered: Arc::new(AtomicBool::new(false)),
        };
        publication.record_failure(error);
        publication
    }
}

fn serve_owned_publication(
    queue: &mut EventQueue<OfferState>,
    state: &mut OfferState,
) -> Result<(), ProtocolError> {
    loop {
        // A failure recorded while preparing, or by the last dispatch, ends
        // serving with that failure even when it also cancelled the offer.
        if let Some(error) = state.publication_failure.take() {
            return Err(error);
        }
        if state.publication_cancelled {
            return Ok(());
        }
        let dispatched = queue.dispatch_pending(state).map_err(|error| {
            ProtocolError::backend_failure(
                format!("serve ordered clipboard publication: dispatch events: {error}"),
                true,
            )
        })?;
        if dispatched > 0 {
            continue;
        }
        queue.flush().map_err(|error| {
            ProtocolError::backend_failure(
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
                ProtocolError::backend_failure(
                    format!("serve ordered clipboard publication: read events: {error}"),
                    true,
                )
            })?;
        }
    }
}

fn wait_for_publication_readable(
    descriptor: std::os::fd::BorrowedFd<'_>,
    timeout: Duration,
) -> Result<bool, ProtocolError> {
    let mut descriptors = [PollFd::new(descriptor, PollFlags::POLLIN)];
    let timeout_ms = timeout.as_millis().clamp(1, u128::from(u16::MAX)) as u16;
    let ready = poll(&mut descriptors, timeout_ms).map_err(|error| {
        ProtocolError::backend_failure(
            format!("serve ordered clipboard publication: wait for events: {error}"),
            true,
        )
    })?;
    if ready == 0 {
        return Ok(false);
    }
    let events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
    if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
        return Err(ProtocolError::backend_failure(
            "serve ordered clipboard publication: Wayland connection became unavailable",
            true,
        ));
    }
    if !events.contains(PollFlags::POLLIN) {
        return Err(ProtocolError::backend_failure(
            "serve ordered clipboard publication: poll returned without readable events",
            true,
        ));
    }
    Ok(true)
}

fn prepare_publication(
    seat_name: &str,
    state: OfferState,
    deadline: Instant,
) -> Result<(EventQueue<OfferState>, OfferState), ProtocolError> {
    let mut discovered = discover(state, deadline, "for clipboard publication")?;
    let seat = discovered.seat(seat_name).ok_or_else(|| {
        ProtocolError::unsupported_capability(format!(
            "clipboard seat {seat_name:?} is unavailable"
        ))
    })?;
    let qh = discovered.queue.handle();
    match discovered.data_control_manager() {
        Some(DataControlManager::Ext(manager)) => {
            let source = manager.create_data_source(&qh, ());
            for (mime_type, _) in &discovered.state.publication {
                source.offer(mime_type.clone());
            }
            let device = manager.get_data_device(&seat, &qh, ());
            require_deadline(deadline, "claim ordered clipboard publication")?;
            device.set_selection(Some(&source));
            discovered.state.owned_source = Some(ClipboardSource::Ext { _proxy: source });
        }
        Some(DataControlManager::Wlr(manager)) => {
            let source = manager.create_data_source(&qh, ());
            for (mime_type, _) in &discovered.state.publication {
                source.offer(mime_type.clone());
            }
            let device = manager.get_data_device(&seat, &qh, ());
            require_deadline(deadline, "claim ordered clipboard publication")?;
            device.set_selection(Some(&source));
            discovered.state.owned_source = Some(ClipboardSource::Wlr { _proxy: source });
        }
        None => {
            return Err(ProtocolError::unsupported_capability(
                "ordered clipboard publication requires ext-data-control or wlr-data-control",
            ));
        }
    }
    discovered.roundtrip(deadline, "acknowledge ordered clipboard publication")?;
    discovered.require_live_device("acknowledge ordered clipboard publication")?;
    // A consumer may already have requested the offer during that roundtrip;
    // its failure must refuse the publication rather than wait for serving.
    if let Some(error) = discovered.state.publication_failure.take() {
        return Err(error);
    }
    Ok((discovered.queue, discovered.state))
}

fn require_deadline(deadline: Instant, operation: &str) -> Result<(), ProtocolError> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(ProtocolError::backend_failure(
            format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
            true,
        ))
    }
}

impl OfferState {
    /// Counts one completed transfer. Returns true when a one-shot offer has
    /// now served its transfers and must withdraw itself.
    fn record_completed_transfer(&mut self) -> bool {
        let Some(remaining) = self.transfers_remaining.as_mut() else {
            return false;
        };
        *remaining = remaining.saturating_sub(1);
        if *remaining > 0 {
            return false;
        }
        if let Some(delivered) = &self.delivered {
            delivered.store(true, Ordering::Release);
        }
        self.publication_cancelled = true;
        true
    }
}

/// How one consumer's transfer request ended.
#[derive(Debug, PartialEq)]
enum Transfer {
    Completed,
    /// The consumer closed its end before reading everything.
    Abandoned,
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
    /// Transfers left before a one-shot offer withdraws itself; `None` for
    /// an offer served until it is replaced.
    transfers_remaining: Option<u32>,
    delivered: Option<Arc<AtomicBool>>,
    device_finished: bool,
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
        proxy: &ZwlrDataControlDeviceV1,
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
            zwlr_data_control_device_v1::Event::Finished => {
                proxy.destroy();
                state.device_finished = true;
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
        proxy: &ExtDataControlDeviceV1,
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
            ext_data_control_device_v1::Event::Finished => {
                proxy.destroy();
                state.device_finished = true;
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
                match serve_publication(&state.publication, &mime_type, fd) {
                    Ok(Transfer::Completed) => {
                        if state.record_completed_transfer() {
                            proxy.destroy();
                        }
                    }
                    Ok(Transfer::Abandoned) => {}
                    Err(error) => {
                        state.publication_failure = Some(error);
                        state.publication_cancelled = true;
                    }
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
                match serve_publication(&state.publication, &mime_type, fd) {
                    Ok(Transfer::Completed) => {
                        if state.record_completed_transfer() {
                            proxy.destroy();
                        }
                    }
                    Ok(Transfer::Abandoned) => {}
                    Err(error) => {
                        state.publication_failure = Some(error);
                        state.publication_cancelled = true;
                    }
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
) -> Result<Transfer, ProtocolError> {
    let (_, data) = representations
        .iter()
        .find(|(offered_mime, _)| offered_mime == mime_type)
        .ok_or_else(|| {
            ProtocolError::backend_failure(
                format!("consumer requested unadvertised clipboard MIME type {mime_type:?}"),
                false,
            )
        })?;
    let mut destination = std::fs::File::from(fd);
    match destination.write_all(data) {
        Ok(()) => Ok(Transfer::Completed),
        // Closing a per-request transfer descriptor abandons only that consumer's
        // read. The advertised offer and its ownership remain valid.
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(Transfer::Abandoned)
        }
        Err(error) => Err(ProtocolError::backend_failure(
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
            return Err(ProtocolError::backend_failure(
                format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
                true,
            ));
        }
        let dispatched = queue.dispatch_pending(state).map_err(|error| {
            ProtocolError::backend_failure(
                format!("{operation}: dispatch Wayland events: {error}"),
                true,
            )
        })?;
        total_dispatched = total_dispatched.saturating_add(dispatched);
        if total_dispatched > MAX_ROUNDTRIP_EVENTS {
            return Err(ProtocolError::backend_failure(
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
                ProtocolError::backend_failure(
                    format!(
                        "{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                    ),
                    true,
                )
            })?;
        queue.flush().map_err(|error| {
            ProtocolError::backend_failure(
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
            ProtocolError::backend_failure(
                format!("{operation}: read Wayland events: {error}"),
                true,
            )
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
        ProtocolError::backend_failure(
            format!("{operation}: wait for Wayland events: {error}"),
            true,
        )
    })?;
    if ready == 0 {
        return Err(ProtocolError::backend_failure(
            format!("{operation} exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"),
            true,
        ));
    }
    let events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
    if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
        return Err(ProtocolError::backend_failure(
            format!("{operation}: Wayland connection became unavailable while waiting for events"),
            true,
        ));
    }
    if !events.contains(PollFlags::POLLIN) {
        return Err(ProtocolError::backend_failure(
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
            ProtocolError::backend_failure(
                format!(
                    "receive clipboard representation exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                ),
                true,
            )
        })?;
        reader
            .set_read_timeout(Some(remaining_time))
            .map_err(|error| {
                ProtocolError::backend_failure(
                    format!("bound clipboard transfer time: {error}"),
                    true,
                )
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
                return Err(ProtocolError::backend_failure(
                    format!(
                        "receive clipboard representation exceeded the {SNAPSHOT_TIMEOUT:?} clipboard snapshot limit"
                    ),
                    true,
                ));
            }
            Err(error) => {
                return Err(ProtocolError::backend_failure(
                    format!("receive clipboard representation: {error}"),
                    true,
                ));
            }
        }
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
        OfferState, SNAPSHOT_TIMEOUT, Transfer, coordinate_publication, read_representation,
        serve_publication, wait_for_publication_readable, wait_for_readable,
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

        let transfer = serve_publication(
            &[("text/plain".into(), b"payload".to_vec())],
            "text/plain",
            writer.into(),
        )
        .unwrap();

        // The offer survives, but the abandoned read delivered nothing.
        assert_eq!(transfer, Transfer::Abandoned);
    }

    #[test]
    fn a_one_shot_offer_counts_only_a_completed_transfer_as_delivery() {
        let delivered = Arc::new(AtomicBool::new(false));
        let mut state = OfferState {
            transfers_remaining: Some(1),
            delivered: Some(Arc::clone(&delivered)),
            ..OfferState::default()
        };
        let mut persistent = OfferState::default();

        assert!(state.record_completed_transfer());
        assert!(delivered.load(Ordering::Acquire));
        assert!(state.publication_cancelled);
        assert!(!persistent.record_completed_transfer());
        assert!(!persistent.publication_cancelled);
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
