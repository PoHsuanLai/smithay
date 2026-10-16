//! Objects held back by [`DataControlHandler::data_control_gate`](super::DataControlHandler::data_control_gate).

use std::fmt;

use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};
use wayland_server::{DisplayHandle, Resource, backend::ClientId};

use crate::wayland::gate::{Gate, Standing};

use super::{DataControlDeviceUserData, DataControlSourceUserData, DataControlState};

/// Registers a held device when the compositor opens it.
type OpenDevice = Box<dyn FnOnce(&DisplayHandle) + Send>;

/// An object that was created while its client was on hold.
pub(super) enum Held {
    /// A data device, with the code that registers it as `get_data_device` would have.
    Device {
        device: ZwlrDataControlDeviceV1,
        open: OpenDevice,
    },
    /// A data source.
    Source(ZwlrDataControlSourceV1),
}

impl fmt::Debug for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Held::Device { device, .. } => f.debug_tuple("Device").field(device).finish(),
            Held::Source(source) => f.debug_tuple("Source").field(source).finish(),
        }
    }
}

impl Held {
    /// Whether the object still exists.
    fn is_alive(&self) -> bool {
        match self {
            Held::Device { device, .. } => device.is_alive(),
            Held::Source(source) => source.is_alive(),
        }
    }

    /// Let the object work as if it had been created open.
    fn open(self, dh: &DisplayHandle) {
        match self {
            Held::Device { device, open } => {
                device_data(&device).standing.set(Standing::Live);
                open(dh);
            }
            Held::Source(source) => source_data(&source).standing.set(Standing::Live),
        }
    }

    /// Tell the client that the object is not available to it, for good.
    pub(super) fn refuse(self) {
        match self {
            Held::Device { device, .. } => {
                device_data(&device).standing.set(Standing::Refused);
                device.finished();
            }
            Held::Source(source) => {
                source_data(&source).standing.set(Standing::Refused);
                source.cancelled();
            }
        }
    }
}

fn device_data(device: &ZwlrDataControlDeviceV1) -> &DataControlDeviceUserData {
    device.data().expect("data control devices carry their user data")
}

fn source_data(source: &ZwlrDataControlSourceV1) -> &DataControlSourceUserData {
    source.data().expect("data control sources carry their user data")
}

impl DataControlState {
    /// Keep `held` for `client`; gives it back when the client has too many on hold already.
    pub(super) fn hold(&mut self, client: ClientId, held: Held) -> Result<(), Held> {
        self.held.hold(client, held, Held::is_alive)
    }

    /// Settle the data devices and sources that `client` created while it was on hold.
    ///
    /// With [`Gate::Open`] each device is registered as `get_data_device` would have registered it
    /// (so it receives the current selection) and each source becomes usable. With
    /// [`Gate::Refuse`] each device is sent `finished` and each source `cancelled`. [`Gate::Hold`]
    /// does nothing. Objects that were destroyed, or whose client is gone, are dropped silently.
    pub fn release(&mut self, dh: &DisplayHandle, client: &ClientId, gate: Gate) {
        if gate == Gate::Hold {
            return;
        }
        for held in self.held.take(client).into_iter().filter(Held::is_alive) {
            match gate {
                Gate::Open => held.open(dh),
                Gate::Refuse => held.refuse(),
                Gate::Hold => {}
            }
        }
    }
}
