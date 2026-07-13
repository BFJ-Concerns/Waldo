use waldo_protocol::ProtocolAdvertisement;
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};

use crate::InspectionError;

struct RegistryState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RegistryState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        // The helper keeps GlobalListContents current. Discovery needs only the
        // initial round trip, so there is no separate local registry state.
    }
}

pub(crate) fn advertised_protocols() -> Result<Vec<ProtocolAdvertisement>, InspectionError> {
    let connection = Connection::connect_to_env()
        .map_err(|error| InspectionError::WaylandConnect(error.to_string()))?;
    let (globals, _queue) = registry_queue_init::<RegistryState>(&connection)
        .map_err(|error| InspectionError::WaylandRegistry(error.to_string()))?;
    let mut protocols = globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|global| ProtocolAdvertisement {
            interface: global.interface,
            version: global.version,
        })
        .collect::<Vec<_>>();
    protocols.sort_by(|left, right| left.interface.cmp(&right.interface));
    Ok(protocols)
}
