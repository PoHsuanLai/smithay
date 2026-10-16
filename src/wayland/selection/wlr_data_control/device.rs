use std::cell::RefCell;
use std::sync::Arc;

use wayland_protocols_wlr::data_control::v1::server::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{Client, DisplayHandle, Resource};

use crate::input::Seat;
use crate::wayland::Dispatch2;
use crate::wayland::gate::{Standing, StandingCell};
use crate::wayland::selection::device::SelectionDevice;
use crate::wayland::selection::offer::OfferReplySource;
use crate::wayland::selection::seat_data::SeatData;
use crate::wayland::selection::source::SelectionSourceProvider;
use crate::wayland::selection::{SelectionSource, SelectionTarget};

use super::{DataControlHandler, DataControlSourceUserData};

#[allow(missing_debug_implementations)]
#[doc(hidden)]
pub struct DataControlDeviceUserData {
    pub(crate) primary_selection_filter: Arc<Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>>,
    pub(crate) wl_seat: WlSeat,
    /// Whether the device works; a held or refused device ignores its requests.
    pub(crate) standing: StandingCell,
}

impl DataControlDeviceUserData {
    /// Stop `device` taking part: it ignores its requests from now on and its seat forgets it.
    pub(super) fn end<D>(&self, device: &ZwlrDataControlDeviceV1)
    where
        D: DataControlHandler + 'static,
    {
        self.standing.set(Standing::Refused);
        if let Some(seat) = Seat::<D>::from_resource(&self.wl_seat) {
            if let Some(seat_data) = seat.user_data().get::<RefCell<SeatData<D::SelectionUserData>>>() {
                seat_data.borrow_mut().retain_devices(|known| match known {
                    SelectionDevice::WlrDataControl(known) => known != device,
                    _ => true,
                });
            }
        }
    }
}

/// Register `device` for `seat`, and send it the current selection(s).
pub(super) fn register_device<D>(
    dh: &DisplayHandle,
    seat: &Seat<D>,
    device: ZwlrDataControlDeviceV1,
    primary: bool,
) where
    D: DataControlHandler,
    D: 'static,
{
    seat.user_data()
        .insert_if_missing(|| RefCell::new(SeatData::<D::SelectionUserData>::new()));

    let device = SelectionDevice::WlrDataControl(device);

    let mut seat_data = seat
        .user_data()
        .get::<RefCell<SeatData<D::SelectionUserData>>>()
        .unwrap()
        .borrow_mut();

    seat_data.add_device(device.clone());

    // NOTE: broadcast selection only to the newly created device.
    let device = Some(&device);
    seat_data.send_selection::<D>(dh, SelectionTarget::Clipboard, device, true);
    if primary {
        seat_data.send_selection::<D>(dh, SelectionTarget::Primary, device, true);
    }
}

/// Whether `source` may be set as a selection: a held or refused source may not.
fn source_is_live(source: &ZwlrDataControlSourceV1) -> bool {
    source
        .data::<DataControlSourceUserData>()
        .is_none_or(|data| data.standing.get() == Standing::Live)
}

impl<D> Dispatch2<ZwlrDataControlDeviceV1, D> for DataControlDeviceUserData
where
    D: DataControlHandler,
    D: 'static,
{
    fn request(
        &self,
        handler: &mut D,
        client: &Client,
        resource: &ZwlrDataControlDeviceV1,
        request: <ZwlrDataControlDeviceV1 as wayland_server::Resource>::Request,
        dh: &DisplayHandle,
        _: &mut wayland_server::DataInit<'_, D>,
    ) {
        if self.standing.get() != Standing::Live {
            return;
        }

        let seat = match Seat::<D>::from_resource(&self.wl_seat) {
            Some(seat) => seat,
            None => return,
        };

        match request {
            zwlr_data_control_device_v1::Request::SetSelection { source, .. } => {
                if source.as_ref().is_some_and(|source| !source_is_live(source)) {
                    return;
                }

                // Each source can only be used once.
                if let Some(source) = source.as_ref() {
                    if handler
                        .data_control_state()
                        .used_sources
                        .insert(source.clone(), self.wl_seat.clone())
                        .is_some()
                    {
                        resource.post_error(
                            zwlr_data_control_device_v1::Error::UsedSource,
                            "selection source can be used only once.",
                        );
                        return;
                    }
                }

                seat.user_data()
                    .insert_if_missing(|| RefCell::new(SeatData::<D::SelectionUserData>::new()));

                let source = source.map(SelectionSourceProvider::WlrDataControl);

                handler.new_selection(
                    SelectionTarget::Clipboard,
                    source.clone().map(|provider| SelectionSource { provider }),
                    seat.clone(),
                );

                seat.user_data()
                    .get::<RefCell<SeatData<D::SelectionUserData>>>()
                    .unwrap()
                    .borrow_mut()
                    .set_clipboard_selection::<D>(dh, source.map(OfferReplySource::Client));
            }
            zwlr_data_control_device_v1::Request::SetPrimarySelection { source, .. } => {
                // When the primary selection is disabled, we should simply ignore the requests.
                if !(*self.primary_selection_filter)(client) {
                    return;
                }

                if source.as_ref().is_some_and(|source| !source_is_live(source)) {
                    return;
                }

                // Each source can only be used once.
                if let Some(source) = source.as_ref() {
                    if handler
                        .data_control_state()
                        .used_sources
                        .insert(source.clone(), self.wl_seat.clone())
                        .is_some()
                    {
                        resource.post_error(
                            zwlr_data_control_device_v1::Error::UsedSource,
                            "selection source can be used only once.",
                        );
                        return;
                    }
                }

                seat.user_data()
                    .insert_if_missing(|| RefCell::new(SeatData::<D::SelectionUserData>::new()));

                let source = source.map(SelectionSourceProvider::WlrDataControl);

                handler.new_selection(
                    SelectionTarget::Primary,
                    source.clone().map(|provider| SelectionSource { provider }),
                    seat.clone(),
                );

                seat.user_data()
                    .get::<RefCell<SeatData<D::SelectionUserData>>>()
                    .unwrap()
                    .borrow_mut()
                    .set_primary_selection::<D>(dh, source.map(OfferReplySource::Client));
            }
            zwlr_data_control_device_v1::Request::Destroy => seat
                .user_data()
                .get::<RefCell<SeatData<D::SelectionUserData>>>()
                .unwrap()
                .borrow_mut()
                .retain_devices(|ndd| match ndd {
                    SelectionDevice::WlrDataControl(ndd) => ndd != resource,
                    _ => true,
                }),

            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        _state: &mut D,
        _client: wayland_server::backend::ClientId,
        resource: &ZwlrDataControlDeviceV1,
    ) {
        if let Some(seat) = Seat::<D>::from_resource(&self.wl_seat) {
            if let Some(seat_data) = seat.user_data().get::<RefCell<SeatData<D::SelectionUserData>>>() {
                seat_data.borrow_mut().retain_devices(|ndd| match ndd {
                    SelectionDevice::WlrDataControl(ndd) => ndd != resource,
                    _ => true,
                });
            }
        }
    }
}
