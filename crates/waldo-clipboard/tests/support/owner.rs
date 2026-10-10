//! A real Wayland clipboard owner. A stalled transfer retains the compositor's
//! descriptor without writing or closing it; it never supplies a read result.
use nix::poll::{PollFd, PollFlags, poll};
use std::{
    io::Write,
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1, zwlr_data_control_source_v1,
};

pub const MIME: &str = "application/x-waldo-read-deadline";

pub struct Owner {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pub transfers: Receiver<()>,
}

impl Owner {
    pub fn start(payload: Option<Vec<u8>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (transfer_tx, transfers) = mpsc::channel();
        let worker = thread::spawn(move || {
            let connection = Connection::connect_to_env().unwrap();
            let (globals, mut queue) = registry_queue_init::<State>(&connection).unwrap();
            let qh = queue.handle();
            let manager: ZwlrDataControlManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).unwrap();
            let source = manager.create_data_source(&qh, ());
            source.offer(MIME.to_owned());
            let device = manager.get_data_device(&seat, &qh, ());
            device.set_selection(Some(&source));
            let mut state = State {
                held: Vec::new(),
                payload,
                transfers: transfer_tx,
            };
            queue.roundtrip(&mut state).unwrap();
            ready_tx.send(()).unwrap();
            while !stopping.load(Ordering::Acquire) {
                queue.dispatch_pending(&mut state).unwrap();
                connection.flush().unwrap();
                if let Some(guard) = queue.prepare_read() {
                    let mut descriptors = [PollFd::new(guard.connection_fd(), PollFlags::POLLIN)];
                    if poll(&mut descriptors, 20_u16).unwrap() > 0 {
                        guard.read().unwrap();
                    }
                }
            }
            source.destroy();
            device.destroy();
            connection.flush().unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Wayland owner must claim its selection");
        Self {
            stop,
            worker: Some(worker),
            transfers,
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct State {
    held: Vec<OwnedFd>,
    payload: Option<Vec<u8>>,
    transfers: mpsc::Sender<()>,
}
impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_source_v1::Event::Send { fd, .. } = event {
            match &state.payload {
                Some(bytes) => std::fs::File::from(fd).write_all(bytes).unwrap(),
                None => state.held.push(fd),
            }
            let _ = state.transfers.send(());
        }
    }
}
macro_rules! ignore_events {
    ($proxy:ty, $event:ty, $data:ty) => {
        impl Dispatch<$proxy, $data> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: $event,
                _: &$data,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}
ignore_events!(
    wl_registry::WlRegistry,
    wl_registry::Event,
    GlobalListContents
);
ignore_events!(wl_seat::WlSeat, wl_seat::Event, ());
ignore_events!(
    ZwlrDataControlManagerV1,
    wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
    ()
);
impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        _: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
    wayland_client::event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ())
    ]);
}
ignore_events!(
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_offer_v1::Event,
    ()
);
