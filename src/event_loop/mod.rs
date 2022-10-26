pub mod control_flow;
pub mod proxy;
pub mod state;

use std::{
    collections::HashMap,
    fmt::Debug,
    mem,
    time::{Duration, Instant},
};

use crate::{
    application::Event,
    dpi::LogicalSize,
    sctk_event::{
        IcedSctkEvent, LayerSurfaceEventVariant, SctkEvent, SeatEventVariant, StartCause,
        SurfaceCompositorUpdate, SurfaceUserRequest, WindowEventVariant,
    },
    settings,
};

use iced_native::command::platform_specific::{self, wayland::layer_surface::IcedLayerSurface};
use sctk::{
    compositor::CompositorState,
    event_loop::WaylandSource,
    output::OutputState,
    reexports::{
        calloop::{self, EventLoop},
        client::{
            backend::ObjectId, globals::registry_queue_init, protocol::wl_surface::WlSurface,
            ConnectError, Connection, DispatchError, Proxy,
        },
    },
    registry::RegistryState,
    seat::SeatState,
    shell::{
        layer::LayerShell,
        xdg::{window::XdgWindowState, XdgShellState},
    },
    shm::ShmState,
};
use wayland_backend::client::WaylandError;

use self::{control_flow::ControlFlow, state::SctkState};

// impl SctkSurface {
//     pub fn hash(&self) -> u64 {
//         let hasher = DefaultHasher::new();
//         match self {
//             SctkSurface::LayerSurface(s) => s.wl_surface().id().hash(.hash(&mut hasher)),
//             SctkSurface::Window(s) => s.wl_surface().id().hash(.hash(&mut hasher)),
//             SctkSurface::Popup(s) => s.wl_surface().id().hash(.hash(&mut hasher)),
//         };
//         hasher.finish()
//     }
// }

#[derive(Debug, Default, Clone, Copy)]
pub struct Features {
    // TODO
}

#[derive(Debug)]
pub struct SctkEventLoop<T> {
    // TODO after merged
    // pub data_device_manager_state: DataDeviceManagerState,
    pub(crate) event_loop: EventLoop<'static, SctkState<T>>,
    pub(crate) wayland_dispatcher:
        calloop::Dispatcher<'static, WaylandSource<SctkState<T>>, SctkState<T>>,
    pub(crate) features: Features,
    /// A proxy to wake up event loop.
    pub event_loop_awakener: calloop::ping::Ping,
    /// A sender for submitting user events in the event loop
    pub user_events_sender: calloop::channel::Sender<Event<T>>,
    pub(crate) state: SctkState<T>,
}

impl<T> SctkEventLoop<T>
where
    T: 'static + Debug,
{
    pub(crate) fn new<F: Sized>(settings: &settings::Settings<F>) -> Result<Self, ConnectError> {
        let connection = Connection::connect_to_env()?;
        let display = connection.display();
        let (globals, mut event_queue) = registry_queue_init(&connection).unwrap();
        let event_loop = calloop::EventLoop::<SctkState<T>>::try_new().unwrap();
        let loop_handle = event_loop.handle();

        let qh = event_queue.handle();
        let registry_state = RegistryState::new(&connection, &qh);

        let (ping, ping_source) = calloop::ping::make_ping().unwrap();
        loop_handle
            .insert_source(ping_source, |_, _, state| {
                // Drain events here as well to account for application doing batch event processing
                // on RedrawEventsCleared.
                // shim::handle_window_requests(state);
                todo!()
            })
            .unwrap();
        let (user_events_sender, user_events_channel) = calloop::channel::channel();

        loop_handle
            .insert_source(user_events_channel, |event, _, state| match event {
                calloop::channel::Event::Msg(e) => {
                    state.pending_user_events.push(e);
                }
                calloop::channel::Event::Closed => {}
            })
            .unwrap();
        let wayland_source = WaylandSource::new(event_queue).unwrap();

        let wayland_dispatcher =
            calloop::Dispatcher::new(wayland_source, |_, queue, winit_state| {
                queue.dispatch_pending(winit_state)
            });

        let _wayland_source_dispatcher = event_loop
            .handle()
            .register_dispatcher(wayland_dispatcher.clone())
            .unwrap();

        Ok(Self {
            event_loop,
            wayland_dispatcher,
            state: SctkState {
                connection,
                registry_state,
                seat_state: SeatState::new(),
                output_state: OutputState::new(),
                compositor_state: CompositorState::bind(&globals, &qh)
                    .expect("wl_compositor is not available"),
                shm_state: ShmState::bind(&globals, &qh).expect("wl_shm is not available"),
                xdg_shell_state: XdgShellState::bind(&globals, &qh)
                    .expect("xdg shell is not available"),
                xdg_window_state: XdgWindowState::bind(&globals, &qh),
                layer_shell: LayerShell::bind(&globals, &qh).expect("layer shell is not available"),

                // data_device_manager_state: DataDeviceManagerState::new(),
                queue_handle: qh,
                loop_handle: loop_handle,

                cursor_surface: None,
                multipool: None,
                outputs: Vec::new(),
                seats: Vec::new(),
                windows: Vec::new(),
                layer_surfaces: Vec::new(),
                popups: Vec::new(),
                kbd_focus: None,
                window_user_requests: HashMap::new(),
                window_compositor_updates: HashMap::new(),
                sctk_events: Vec::new(),
                popup_compositor_updates: Default::default(),
                layer_surface_compositor_updates: Default::default(),
                layer_surface_user_requests: Default::default(),
                popup_user_requests: Default::default(),
                pending_user_events: Vec::new(),
            },
            features: Default::default(),
            event_loop_awakener: ping,
            user_events_sender,
        })
    }

    pub fn proxy(&self) -> proxy::Proxy<Event<T>> {
        proxy::Proxy::new(self.user_events_sender.clone())
    }

    pub fn get_layer_surface(
        &mut self,
        layer_surface: IcedLayerSurface,
    ) -> (iced_native::window::Id, WlSurface) {
        self.state.get_layer_surface(layer_surface)
    }

    pub fn run_return<F>(&mut self, mut callback: F) -> i32
    where
        F: FnMut(IcedSctkEvent<T>, &SctkState<T>, &mut ControlFlow),
    {
        let mut control_flow = ControlFlow::Poll;

        callback(
            IcedSctkEvent::NewEvents(StartCause::Init),
            &self.state,
            &mut control_flow,
        );

        let mut window_compositor_updates: Vec<(ObjectId, SurfaceCompositorUpdate)> = Vec::new();
        let mut surface_user_requests: Vec<(ObjectId, SurfaceUserRequest)> = Vec::new();

        let mut popup_compositor_updates: Vec<(ObjectId, SurfaceCompositorUpdate)> = Vec::new();
        let mut popup_user_requests: Vec<(ObjectId, SurfaceUserRequest)> = Vec::new();

        let mut layer_surface_compositor_updates: Vec<(ObjectId, SurfaceCompositorUpdate)> =
            Vec::new();
        let mut layer_surface_user_requests: Vec<(ObjectId, SurfaceUserRequest)> = Vec::new();

        let mut event_sink_back_buffer = Vec::new();

        // NOTE We break on errors from dispatches, since if we've got protocol error
        // libwayland-client/wayland-rs will inform us anyway, but crashing downstream is not
        // really an option. Instead we inform that the event loop got destroyed. We may
        // communicate an error that something was terminated, but winit doesn't provide us
        // with an API to do that via some event.
        // Still, we set the exit code to the error's OS error code, or to 1 if not possible.
        let exit_code = loop {
            // Send pending events to the server.
            let _ = self.state.connection.flush();

            // During the run of the user callback, some other code monitoring and reading the
            // Wayland socket may have been run (mesa for example does this with vsync), if that
            // is the case, some events may have been enqueued in our event queue.
            //
            // If some messages are there, the event loop needs to behave as if it was instantly
            // woken up by messages arriving from the Wayland socket, to avoid delaying the
            // dispatch of these events until we're woken up again.
            let instant_wakeup = {
                let mut wayland_source = self.wayland_dispatcher.as_source_mut();
                let queue = wayland_source.queue();
                match queue.dispatch_pending(&mut self.state) {
                    Ok(dispatched) => dispatched > 0,
                    // TODO better error handling
                    Err(error) => {
                        break match error {
                            DispatchError::BadMessage { .. } => None,
                            DispatchError::Backend(err) => match err {
                                WaylandError::Io(err) => err.raw_os_error(),
                                WaylandError::Protocol(_) => None,
                            },
                        }
                        .unwrap_or(1)
                    }
                }
            };

            match control_flow {
                ControlFlow::ExitWithCode(code) => break code,
                ControlFlow::Poll => {
                    // Non-blocking dispatch.
                    let timeout = Duration::from_millis(0);
                    if let Err(error) = self.event_loop.dispatch(Some(timeout), &mut self.state) {
                        break raw_os_err(error);
                    }

                    callback(
                        IcedSctkEvent::NewEvents(StartCause::Poll),
                        &self.state,
                        &mut control_flow,
                    );
                }
                ControlFlow::Wait => {
                    let timeout = if instant_wakeup {
                        Some(Duration::from_millis(0))
                    } else {
                        None
                    };

                    if let Err(error) = self.event_loop.dispatch(timeout, &mut self.state) {
                        break raw_os_err(error);
                    }

                    callback(
                        IcedSctkEvent::NewEvents(StartCause::WaitCancelled {
                            start: Instant::now(),
                            requested_resume: None,
                        }),
                        &self.state,
                        &mut control_flow,
                    );
                }
                ControlFlow::WaitUntil(deadline) => {
                    let start = Instant::now();

                    // Compute the amount of time we'll block for.
                    let duration = if deadline > start && !instant_wakeup {
                        deadline - start
                    } else {
                        Duration::from_millis(0)
                    };

                    if let Err(error) = self.event_loop.dispatch(Some(duration), &mut self.state) {
                        break raw_os_err(error);
                    }

                    let now = Instant::now();

                    if now < deadline {
                        callback(
                            IcedSctkEvent::NewEvents(StartCause::WaitCancelled {
                                start,
                                requested_resume: Some(deadline),
                            }),
                            &self.state,
                            &mut control_flow,
                        )
                    } else {
                        callback(
                            IcedSctkEvent::NewEvents(StartCause::ResumeTimeReached {
                                start,
                                requested_resume: deadline,
                            }),
                            &self.state,
                            &mut control_flow,
                        )
                    }
                }
            }

            self.handle_pending_user_events(&mut control_flow, &mut callback);

            // Process 'new' pending updates from compositor.
            window_compositor_updates.clear();
            window_compositor_updates.extend(
                self.state
                    .window_compositor_updates
                    .iter_mut()
                    .map(|(wid, window_update)| (wid.clone(), mem::take(window_update))),
            );

            for (window_id, window_compositor_update) in window_compositor_updates.iter_mut() {
                if let Some(scale_factor) = window_compositor_update.scale_factor.map(|f| f as f64)
                {
                    let (physical_size, configure) = {
                        let window_handle = self
                            .state
                            .windows
                            .iter_mut()
                            .find(|w| w.window.wl_surface().id() == *window_id)
                            .unwrap();
                        let mut size = window_handle.current_size.as_mut().unwrap();

                        // Update the new logical size if it was changed.
                        let configure = window_compositor_update.configure.as_ref();
                        let window_size = configure
                            .and_then(|c| (c.new_size))
                            .unwrap_or((size.width, size.height));
                        *size = LogicalSize::new(window_size.0, window_size.1);

                        (
                            LogicalSize::new(window_size.0, window_size.1)
                                .to_physical(scale_factor),
                            configure.unwrap(),
                        )
                    };

                    sticky_exit_callback(
                        IcedSctkEvent::SctkEvent(SctkEvent::ScaleFactorChanged {
                            id: window_id.clone(),
                            factor: scale_factor,
                            inner_size: physical_size,
                        }),
                        &self.state,
                        &mut control_flow,
                        &mut callback,
                    );

                    // We don't update size on a window handle since we'll do that later
                    // when handling size update.
                    let new_logical_size = physical_size.to_logical::<u32>(scale_factor);
                    window_compositor_update.configure = Some(configure.clone());
                }

                if let Some(configure) = window_compositor_update.configure.take() {
                    let physical_size = {
                        let window_handle = self
                            .state
                            .windows
                            .iter_mut()
                            .find(|w| w.window.wl_surface().id() == *window_id)
                            .unwrap();
                        let mut window_size = window_handle.current_size.as_mut().unwrap();
                        let size = configure
                            .new_size
                            .map(|c| LogicalSize {
                                width: c.0,
                                height: c.1,
                            })
                            .unwrap_or(*window_size);
                        // Always issue resize event on scale factor change.
                        let physical_size = if window_compositor_update.scale_factor.is_none()
                            && *window_size == size
                        {
                            // The size hasn't changed, don't inform downstream about that.
                            None
                        } else {
                            *window_size = size;
                            let physical_size = size.to_physical::<u32>(
                                window_compositor_update.scale_factor.unwrap() as f64,
                            );
                            Some(physical_size)
                        };

                        // We still perform all of those resize related logic even if the size
                        // hasn't changed, since GNOME relies on `set_geometry` calls after
                        // configures.
                        window_handle.window.xdg_surface().set_window_geometry(
                            0,
                            0,
                            size.width as i32,
                            size.height as i32,
                        );
                        window_handle.window.wl_surface().commit();

                        // Mark that refresh isn't required, since we've done it right now.
                        self.state
                            .window_user_requests
                            .get_mut(window_id)
                            .unwrap()
                            .refresh_frame = false;

                        physical_size
                    };

                    if let Some(physical_size) = physical_size {
                        sticky_exit_callback(
                            IcedSctkEvent::SctkEvent(SctkEvent::WindowEvent {
                                variant: WindowEventVariant::Configure(configure),
                                id: window_id.clone(),
                            }),
                            &self.state,
                            &mut control_flow,
                            &mut callback,
                        );
                    }
                }

                // If the close is requested, send it here.
                if window_compositor_update.close_window {
                    sticky_exit_callback(
                        IcedSctkEvent::SctkEvent(SctkEvent::WindowEvent {
                            variant: WindowEventVariant::Close,
                            id: window_id.clone(),
                        }),
                        &self.state,
                        &mut control_flow,
                        &mut callback,
                    );
                }
            }

            // The purpose of the back buffer and that swap is to not hold borrow_mut when
            // we're doing callback to the user, since we can double borrow if the user decides
            // to create a window in one of those callbacks.
            std::mem::swap(&mut event_sink_back_buffer, &mut self.state.sctk_events);

            // Handle pending sctk events.
            let mut must_redraw = Vec::new();

            for event in event_sink_back_buffer.drain(..) {
                match event {
                    SctkEvent::SeatEvent { variant, id } => {} // pretty sure i can ignore these for now and maybe remove them until we want iced to support multiseat...
                    SctkEvent::PointerEvent {
                        variant,
                        ptr_id,
                        seat_id,
                    } => {
                        sticky_exit_callback(
                            IcedSctkEvent::SctkEvent(SctkEvent::PointerEvent { variant, ptr_id, seat_id }),
                            &self.state,
                            &mut control_flow,
                            &mut callback,
                        );
                    },
                    SctkEvent::KeyboardEvent {
                        variant,
                        kbd_id,
                        seat_id,
                    } => {
                        
                    },
                    SctkEvent::WindowEvent { variant, id } => todo!(),
                    SctkEvent::LayerSurfaceEvent { variant, id } => match variant {
                        crate::sctk_event::LayerSurfaceEventVariant::Created(_) => todo!(),
                        crate::sctk_event::LayerSurfaceEventVariant::Done => todo!(),
                        crate::sctk_event::LayerSurfaceEventVariant::Configure(configure) => {
                            sticky_exit_callback(
                                IcedSctkEvent::SctkEvent(SctkEvent::LayerSurfaceEvent {
                                    variant: LayerSurfaceEventVariant::Configure(configure),
                                    id,
                                }),
                                &self.state,
                                &mut control_flow,
                                &mut callback,
                            );
                        }
                    },
                    SctkEvent::PopupEvent {
                        variant,
                        toplevel_id,
                        parent_id,
                        id,
                    } => todo!(),
                    SctkEvent::NewOutput { id, info } => {
                        // TODO forward output events so the user can create custom layer surfaces for them
                    }
                    SctkEvent::UpdateOutput { id, info } => {
                        // TODO forward output events so the user can create custom layer surfaces for them
                    }
                    SctkEvent::RemovedOutput(_) => todo!(),
                    SctkEvent::Draw(id) => must_redraw.push(id),
                    SctkEvent::ScaleFactorChanged {
                        factor,
                        id,
                        inner_size,
                    } => todo!(),
                }
                // sticky_exit_callback(event, &self.state, &mut control_flow, &mut callback);
            }

            // Send events cleared.
            sticky_exit_callback(
                IcedSctkEvent::MainEventsCleared,
                &self.state,
                &mut control_flow,
                &mut callback,
            );

            // Apply user requests, so every event required resize and latter surface commit will
            // be applied right before drawing. This will also ensure that every `RedrawRequested`
            // event will be delivered in time.
            // shim::handle_window_requests(self.state);
            // TODO
            // Process 'new' pending updates from compositor.
            surface_user_requests.clear();
            surface_user_requests.extend(
                self.state
                    .window_user_requests
                    .iter_mut()
                    .map(|(wid, window_request)| (wid.clone(), mem::take(window_request))),
            );

            // Handle RedrawRequested requests.
            for (surface_id, mut surface_request) in surface_user_requests.iter() {
                if let Some(i) = must_redraw.iter().position(|a_id| a_id == surface_id) {
                    must_redraw.remove(i);
                }
                // Handle refresh of the frame.
                if surface_request.refresh_frame {
                    //TODO
                    let window_handle = self
                        .state
                        .windows
                        .iter()
                        .find(|w| w.window.wl_surface().id() == *surface_id)
                        .unwrap();
                    // window_handle.window.refresh();

                    // In general refreshing the frame requires surface commit, those force user
                    // to redraw.
                    surface_request.redraw_requested = true;
                }

                // Handle redraw request.
                if surface_request.redraw_requested {
                    sticky_exit_callback(
                        IcedSctkEvent::RedrawRequested(surface_id.clone()),
                        &self.state,
                        &mut control_flow,
                        &mut callback,
                    );
                }
            }

            for id in must_redraw {
                sticky_exit_callback(
                    IcedSctkEvent::RedrawRequested(id.clone()),
                    &self.state,
                    &mut control_flow,
                    &mut callback,
                );
                if let Some(layer_surface) = self.state.layer_surfaces.iter().find(|l| l.surface.wl_surface().id() == id) {
                    layer_surface.surface.wl_surface().commit();
                }
            }

            // Send RedrawEventCleared.
            sticky_exit_callback(
                IcedSctkEvent::RedrawEventsCleared,
                &self.state,
                &mut control_flow,
                &mut callback,
            );
        };

        callback(IcedSctkEvent::LoopDestroyed, &self.state, &mut control_flow);
        exit_code
    }

    fn handle_pending_user_events<F: FnMut(IcedSctkEvent<T>, &SctkState<T>, &mut ControlFlow)>(&mut self, control_flow: &mut ControlFlow, callback: &mut F) {
            // Handle pending user events one more time, just in case anything new has been requested in response to the sctk events
            // user events indirectly via callback to the user.
            let user_events = self.state.pending_user_events.drain(..).collect::<Vec<_>>();
            for user_event in user_events {
                match user_event {
                    Event::SctkEvent(event) => {
                        sticky_exit_callback(event, &self.state, control_flow, callback)
                    }
                    Event::LayerSurface(
                        platform_specific::wayland::layer_surface::Action::LayerSurface {
                            builder,
                            ..
                        },
                    ) => {
                        todo!()
                    }
                    Event::LayerSurface(
                        platform_specific::wayland::layer_surface::Action::Size {
                            id,
                            width,
                            height,
                        },
                    ) => {
                        todo!()
                    }
                    Event::SetCursor(_) => {
                        // TODO set cursor after cursor theming PR is merged
                        // https://github.com/Smithay/client-toolkit/pull/306
                    },
                }
            }
    }
}



fn sticky_exit_callback<T, F>(
    evt: IcedSctkEvent<T>,
    target: &SctkState<T>,
    control_flow: &mut ControlFlow,
    callback: &mut F,
) where
    F: FnMut(IcedSctkEvent<T>, &SctkState<T>, &mut ControlFlow),
{
    // make ControlFlow::ExitWithCode sticky by providing a dummy
    // control flow reference if it is already ExitWithCode.
    if let ControlFlow::ExitWithCode(code) = *control_flow {
        callback(evt, target, &mut ControlFlow::ExitWithCode(code))
    } else {
        callback(evt, target, control_flow)
    }
}

fn raw_os_err(err: calloop::Error) -> i32 {
    match err {
        calloop::Error::IoError(err) => err.raw_os_error(),
        _ => None,
    }
    .unwrap_or(1)
}
