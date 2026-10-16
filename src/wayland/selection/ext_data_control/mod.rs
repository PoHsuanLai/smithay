//! Automatic handling of the `ext_data_control` protocol
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
//! use smithay::wayland::selection::ext_data_control::{DataControlState, DataControlHandler};
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

use wayland_protocols::ext::data_control::v1::server::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::server::ext_data_control_source_v1::ExtDataControlSourceV1;
use wayland_server::backend::{ClientId, GlobalId};
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::{Client, DisplayHandle, GlobalDispatch};

mod device;
mod held;
mod source;

pub use device::ExtDataControlDeviceUserData;
pub use source::ExtDataControlSourceUserData;

use super::SelectionHandler;
use super::primary_selection::PrimarySelectionState;
use crate::wayland::gate::{Gate, HeldByClient};
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
    pub(crate) used_sources: HashMap<ExtDataControlSourceV1, WlSeat>,
    /// Devices and sources that clients created while on hold, by client.
    held: HeldByClient<ClientId, Held>,
}

impl DataControlState {
    /// Register new [ExtDataControlManagerV1] global.
    ///
    /// Passing `primary_selection` will enable support for primary selection as well.
    pub fn new<D, F>(
        display: &DisplayHandle,
        primary_selection: Option<&PrimarySelectionState>,
        filter: F,
    ) -> Self
    where
        D: GlobalDispatch<ExtDataControlManagerV1, ExtDataControlManagerGlobalData> + 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let data = ExtDataControlManagerGlobalData {
            primary: primary_selection.is_some(),
            filter: Box::new(filter),
        };
        let manager_global = display.create_global::<D, ExtDataControlManagerV1, _>(1, data);
        Self {
            manager_global,
            used_sources: Default::default(),
            held: Default::default(),
        }
    }

    /// [ExtDataControlManagerV1]  GlobalId getter.
    pub fn global(&self) -> GlobalId {
        self.manager_global.clone()
    }
}

#[allow(missing_debug_implementations)]
#[doc(hidden)]
pub struct ExtDataControlManagerGlobalData {
    /// Whether to allow primary selection.
    primary: bool,

    /// Filter whether the clients can view global.
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct ExtDataControlManagerUserData {
    /// Whether to allow primary selection.
    primary: bool,
}

mod handlers {
    use tracing::error;
    use wayland_protocols::ext::data_control::v1::server::{
        ext_data_control_device_v1::ExtDataControlDeviceV1,
        ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
        ext_data_control_source_v1::ExtDataControlSourceV1,
    };
    use wayland_server::{Client, Dispatch, DisplayHandle};

    use crate::input::Seat;
    use crate::wayland::gate::{Gate, Standing, StandingCell};
    use crate::wayland::{Dispatch2, GlobalDispatch2};

    use super::device::register_device;
    use super::held::Held;

    use super::DataControlHandler;
    use super::ExtDataControlDeviceUserData;
    use super::ExtDataControlManagerGlobalData;
    use super::ExtDataControlManagerUserData;
    use super::ExtDataControlSourceUserData;

    impl<D> GlobalDispatch2<ExtDataControlManagerV1, D> for ExtDataControlManagerGlobalData
    where
        D: Dispatch<ExtDataControlManagerV1, ExtDataControlManagerUserData>,
        D: DataControlHandler,
        D: 'static,
    {
        fn bind(
            &self,
            _state: &mut D,
            _handle: &DisplayHandle,
            _client: &wayland_server::Client,
            resource: wayland_server::New<ExtDataControlManagerV1>,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            data_init.init(
                resource,
                ExtDataControlManagerUserData {
                    primary: self.primary,
                },
            );
        }

        fn can_view(&self, client: &Client) -> bool {
            (self.filter)(client)
        }
    }

    impl<D> Dispatch2<ExtDataControlManagerV1, D> for ExtDataControlManagerUserData
    where
        D: Dispatch<ExtDataControlDeviceV1, ExtDataControlDeviceUserData>,
        D: Dispatch<ExtDataControlSourceV1, ExtDataControlSourceUserData>,
        D: DataControlHandler,
        D: 'static,
    {
        fn request(
            &self,
            handler: &mut D,
            client: &wayland_server::Client,
            _resource: &ExtDataControlManagerV1,
            request: <ExtDataControlManagerV1 as wayland_server::Resource>::Request,
            dh: &DisplayHandle,
            data_init: &mut wayland_server::DataInit<'_, D>,
        ) {
            match request {
                ext_data_control_manager_v1::Request::CreateDataSource { id } => {
                    let gate = handler.data_control_gate(client);
                    let source =
                        data_init.init(id, ExtDataControlSourceUserData::new(dh.clone(), gate.into()));
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
                ext_data_control_manager_v1::Request::GetDataDevice { id, seat: wl_seat } => {
                    let gate = handler.data_control_gate(client);
                    if gate == Gate::Open {
                        match Seat::<D>::from_resource(&wl_seat) {
                            Some(seat) => {
                                let device = data_init.init(
                                    id,
                                    ExtDataControlDeviceUserData {
                                        wl_seat,
                                        primary: self.primary,
                                        standing: StandingCell::new(Standing::Live),
                                    },
                                );
                                register_device::<D>(dh, &seat, device, self.primary);
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
                        ExtDataControlDeviceUserData {
                            wl_seat: wl_seat.clone(),
                            primary: self.primary,
                            standing: StandingCell::new(gate.into()),
                        },
                    );
                    let primary = self.primary;
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
                ext_data_control_manager_v1::Request::Destroy => (),
                _ => unreachable!(),
            }
        }
    }
}
