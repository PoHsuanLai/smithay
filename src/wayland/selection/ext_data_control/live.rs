//! Ending the data devices of a client, see [`DataControlState::revoke`].

use wayland_protocols::ext::data_control::v1::server::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_server::{Resource, backend::ClientId};

use super::{DataControlHandler, DataControlState, ExtDataControlDeviceUserData};

impl DataControlState {
    /// Remember a device that was registered with its seat.
    pub(super) fn track(&mut self, device: ExtDataControlDeviceV1) {
        self.live.track(device, |device| device.is_alive());
    }

    /// Take back the power to use data control from `client`, which had been given it.
    ///
    /// Every data device of the client is ended: it is taken off its seat, so selections are no
    /// longer sent to it, it ignores its requests from now on, and it is sent `finished`. Objects
    /// the client still has on hold are refused as [`DataControlState::release`] with
    /// [`Gate::Refuse`](crate::wayland::Gate::Refuse) would. A selection the client already set
    /// stays until another client replaces it, and its sources are not ended.
    ///
    /// The client can ask again: its next request goes through
    /// [`DataControlHandler::data_control_gate`] afresh.
    pub fn revoke<D>(&mut self, client: &ClientId)
    where
        D: DataControlHandler + 'static,
    {
        for held in self
            .held
            .take(client)
            .into_iter()
            .filter(super::held::Held::is_alive)
        {
            held.refuse();
        }
        let ended = self.live.end(
            |device| device.is_alive(),
            |device| device.client().is_some_and(|owner| owner.id() == *client),
        );
        for device in ended {
            if let Some(data) = device.data::<ExtDataControlDeviceUserData>() {
                data.end::<D>(&device);
            }
            device.finished();
        }
    }
}
