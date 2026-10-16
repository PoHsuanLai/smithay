//! Automatic handling of the `wlr_data_control` protocol
//!
//! ## Initialization
//!
//! To initialize this implementation, create [`DataControlState`], store it in your `State`
//! struct, and implement the required trait, as shown in the example:
//!
//! ```
//! # extern crate wayland_server;
//! # #[macro_use] extern crate smithay;
//! # use smithay::wayland::compositor::{CompositorHandler, CompositorState, CompositorClientState};
//! use smithay::wayland::selection::SelectionHandler;
//! use smithay::wayland::selection::wlr_data_control::{DataControlState, DataControlHandler};
//! # use smithay::input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus};
//! # use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
//! # use smithay::wayland::pointer_constraints::PointerConstraintsHandler;
//!
//! # struct State { data_control_state: DataControlState }
//! # let mut display = wayland_server::Display::<State>::new().unwrap();
//! // Create the data_control state
//! let data_control_state = DataControlState::new::<State, _>(
//!     &display.handle(), None, |_| true
//! );
//!
//! // insert the DataControlState into your state
//! // ..
//!
//! // implement the necessary traits
//! # impl CompositorHandler for State {
//! #     fn compositor_state(&mut self) -> &mut CompositorState { unimplemented!() }
//! #     fn client_compositor_state<'a>(&self, client: &'a wayland_server::Client) -> &'a CompositorClientState { unimplemented!() }
//! #     fn commit(&mut self, surface: &wayland_server::protocol::wl_surface::WlSurface) {}
//! # }
//! # impl SeatHandler for State {
//! #     type KeyboardFocus = WlSurface;
//! #     type PointerFocus = WlSurface;
//! #     type TouchFocus = WlSurface;
//! #     fn seat_state(&mut self) -> &mut SeatState<Self> { unimplemented!() }
//! #     fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) { unimplemented!() }
//! #     fn cursor_image(&mut self, seat: &Seat<Self>, image: CursorImageStatus) { unimplemented!() }
//! # }
//! # impl PointerConstraintsHandler for State {}
//! impl SelectionHandler for State {
//!     type SelectionUserData = ();
//! }
//! impl DataControlHandler for State {
//!     fn data_control_state(&mut self) -> &mut DataControlState { &mut self.data_control_state }
//!     // ... override default implementations here to customize handling ...
//! }
//!
//! smithay::delegate_dispatch2!(State);
//!
//! // You're now ready to go!
//! ```
//!
//! Be aware that data control clients rely on other selection providers to be implemneted, like
//! wl_data_device or zwp_primary_selection.

use std::collections::HashMap;
use std::sync::Arc;

use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;
use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;
use wayland_server::backend::{ClientId, GlobalId};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{Client, DisplayHandle, GlobalDispatch};

mod device;
mod held;
mod live;
mod source;

pub use device::DataControlDeviceUserData;
pub use source::DataControlSourceUserData;

use super::SelectionHandler;
use super::primary_selection::PrimarySelectionState;
use crate::wayland::gate::{Gate, HeldByClient, LiveObjects};
use held::Held;

/// Access the data control state.
pub trait DataControlHandler: Sized + SelectionHandler {
    /// [`DataControlState`] getter.
    fn data_control_state(&mut self) -> &mut DataControlState;

    /// Whether `client` may use data control (watch and set the clipboard).
    ///
    /// Asked when the client creates a data device or a data source. The default is
    /// [`Gate::Open`]. See [`Gate`] for what [`Gate::Hold`] and [`Gate::Refuse`] do here; held
    /// objects are settled with [`DataControlState::release`].
    fn data_control_gate(&mut self, client: &Client) -> Gate {
        let _ = client;
        Gate::Open
    }
}

/// State of the data control.
#[derive(Debug)]
pub struct DataControlState {
    manager_global: GlobalId,
    /// Used sources.
    ///
    /// Protocol states that each source can only be used once. We
    /// also use it during destruction to get seat data.
    pub(crate) used_sources: HashMap<ZwlrDataControlSourceV1, WlSeat>,
    /// Devices and sources that clients created while on hold, by client.
    held: HeldByClient<ClientId, Held>,
    /// Devices that are registered with a seat, so that a revocation can end them.
    live: LiveObjects<ZwlrDataControlDeviceV1>,
}

impl DataControlState {
    /// Register new [ZwlrDataControlManagerV1] global.
    ///
    /// Passing `primary_selection` will enable support for primary selection as well.
    pub fn new<D, F>(
        display: &DisplayHandle,
        primary_selection: Option<&PrimarySelectionState>,
        filter: F,
    ) -> Self
    where
        D: GlobalDispatch<ZwlrDataControlManagerV1, DataControlManagerGlobalData> + 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let data = DataControlManagerGlobalData {
            primary_selection_filter: primary_selection
                .map(|x| Arc::clone(&x.filter))
                .unwrap_or_else(|| Arc::new(Box::new(|_| false))),
            filter: Box::new(filter),
        };
        let manager_global = display.create_global::<D, ZwlrDataControlManagerV1, _>(2, data);
        Self {
            manager_global,
            used_sources: Default::default(),
            held: Default::default(),
            live: Default::default(),
        }
    }

    /// [ZwlrDataControlManagerV1]  GlobalId getter.
    pub fn global(&self) -> GlobalId {
        self.manager_global.clone()
    }
}

#[allow(missing_debug_implementations)]
#[doc(hidden)]
pub struct DataControlManagerGlobalData {
    /// Filter whether the clients can read/modify the primary selection.
    primary_selection_filter: Arc<Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>>,

    /// Filter whether the clients can view global.
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

#[allow(missing_debug_implementations)]
#[doc(hidden)]
#[derive(Clone)]
pub struct DataControlManagerUserData {
    /// Filter whether the clients can read/modify the primary selection.
    primary_selection_filter: Arc<Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>>,
}

mod handlers {
    use std::sync::Arc;

    use tracing::error;
    use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;
    use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1;
    use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
    use wayland_protocols_wlr::data_control::v1::server::zwlr_data_control_source_v1::ZwlrDataControlSourceV1;
    use wayland_server::{Client, Dispatch, DisplayHandle};

    use crate::input::Seat;
    use crate::wayland::gate::{Gate, Standing, StandingCell};
    use crate::wayland::{Dispatch2, GlobalDispatch2};

    use super::device::register_device;
    use super::held::Held;

    use super::DataControlDeviceUserData;
    use super::DataControlHandler;
    use super::DataControlManagerGlobalData;
    use super::DataControlManagerUserData;
    use super::DataControlSourceUserData;

    impl<D> GlobalDispatch2<ZwlrDataControlManagerV1, D> for DataControlManagerGlobalData
    where
        D: Dispatch<ZwlrDataControlManagerV1, DataControlManagerUserData>,
        D: Dispatch<ZwlrDataControlDeviceV1, DataControlDeviceUserData>,
        D: Dispatch<ZwlrDataControlSourceV1, DataControlSourceUserData>,
        D: DataControlHandler,
        D: 'static,
    {
        fn bind(
            &self,
            _state: &mut D,
            _handle: &DisplayHandle,
            _client: &wayland_server::Client,
            resource: wayland_server::New<ZwlrDataControlManagerV1>,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            data_init.init(
                resource,
                DataControlManagerUserData {
                    primary_selection_filter: Arc::clone(&self.primary_selection_filter),
                },
            );
        }

        fn can_view(&self, client: &Client) -> bool {
            (self.filter)(client)
        }
    }

    impl<D> Dispatch2<ZwlrDataControlManagerV1, D> for DataControlManagerUserData
    where
        D: Dispatch<ZwlrDataControlDeviceV1, DataControlDeviceUserData>,
        D: Dispatch<ZwlrDataControlSourceV1, DataControlSourceUserData>,
        D: DataControlHandler,
        D: 'static,
    {
        fn request(
            &self,
            handler: &mut D,
            client: &wayland_server::Client,
            _resource: &ZwlrDataControlManagerV1,
            request: <ZwlrDataControlManagerV1 as wayland_server::Resource>::Request,
            dh: &DisplayHandle,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            match request {
                zwlr_data_control_manager_v1::Request::CreateDataSource { id } => {
                    let gate = handler.data_control_gate(client);
                    let source = data_init.init(id, DataControlSourceUserData::new(dh.clone(), gate.into()));
                    match gate {
                        Gate::Open => (),
                        Gate::Refuse => Held::Source(source).refuse(),
                        Gate::Hold => {
                            if let Err(held) = handler
                                .data_control_state()
                                .hold(client.id(), Held::Source(source))
                            {
                                held.refuse();
                            }
                        }
                    }
                }
                zwlr_data_control_manager_v1::Request::GetDataDevice { id, seat: wl_seat } => {
                    let gate = handler.data_control_gate(client);
                    if gate == Gate::Open {
                        match Seat::<D>::from_resource(&wl_seat) {
                            Some(seat) => {
                                let device = data_init.init(
                                    id,
                                    DataControlDeviceUserData {
                                        wl_seat,
                                        primary_selection_filter: Arc::clone(&self.primary_selection_filter),
                                        standing: StandingCell::new(Standing::Live),
                                    },
                                );
                                let primary = (*self.primary_selection_filter)(client);
                                handler.data_control_state().track(device.clone());
                                register_device::<D>(dh, &seat, device, primary);
                            }
                            None => {
                                error!(
                                    data_control_device = ?id,
                                    client = ?client,
                                    "Unmanaged seat given to a primary selection device."
                                );
                            }
                        }
                        return;
                    }

                    let device = data_init.init(
                        id,
                        DataControlDeviceUserData {
                            wl_seat: wl_seat.clone(),
                            primary_selection_filter: Arc::clone(&self.primary_selection_filter),
                            standing: StandingCell::new(gate.into()),
                        },
                    );
                    let primary = (*self.primary_selection_filter)(client);
                    let open_device = device.clone();
                    let held = Held::Device {
                        device,
                        open: Box::new(move |dh: &DisplayHandle| {
                            if let Some(seat) = Seat::<D>::from_resource(&wl_seat) {
                                register_device::<D>(dh, &seat, open_device, primary);
                            }
                        }),
                    };
                    match gate {
                        Gate::Hold => {
                            if let Err(held) = handler.data_control_state().hold(client.id(), held) {
                                held.refuse();
                            }
                        }
                        _ => held.refuse(),
                    }
                }
                zwlr_data_control_manager_v1::Request::Destroy => (),
                _ => unreachable!(),
            }
        }
    }
}
