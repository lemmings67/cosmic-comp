// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    backend::render::cursor::CursorState,
    config::{xkb_config_to_wl, Action, Config, KeyPattern, WorkspaceLayout},
    shell::{
        focus::{target::PointerFocusTarget, FocusDirection},
        grabs::{ResizeEdge, SeatMoveGrabState},
        layout::tiling::{SwapWindowGrab, TilingLayout},
        Direction, FocusResult, MoveResult, OverviewMode, ResizeDirection, ResizeMode, Trigger,
        Workspace,
    },
    state::Common,
    utils::prelude::*,
    wayland::{handlers::screencopy::ScreencopySessions, protocols::screencopy::Session},
};
use calloop::{timer::Timer, RegistrationToken};
use cosmic_protocols::screencopy::v1::server::zcosmic_screencopy_session_v1::InputType;
#[allow(deprecated)]
use smithay::{
    backend::input::{
        Axis, AxisSource, Device, DeviceCapability, GestureBeginEvent, GestureEndEvent,
        GesturePinchUpdateEvent as _, GestureSwipeUpdateEvent as _, InputBackend, InputEvent,
        KeyState, PointerAxisEvent,
    },
    desktop::{layer_map_for_output, space::SpaceElement, WindowSurfaceType},
    input::{
        keyboard::{keysyms, FilterResult, KeysymHandle, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
            GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
            GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent,
            RelativeMotionEvent,
        },
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        input::event::pointer::PointerAxisEvent as LibinputPointerAxisEvent,
        wayland_server::DisplayHandle,
    },
    utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER},
    wayland::{
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat, seat::WaylandFocus,
        shell::wlr_layer::Layer as WlrLayer,
    },
    xwayland::X11Surface,
};
#[cfg(not(feature = "debug"))]
use tracing::info;
use tracing::{error, trace, warn};

use std::{
    any::Any,
    cell::RefCell,
    collections::HashMap,
    time::{Duration, Instant},
};
use xkbcommon::xkb::KEY_XF86Switch_VT_12;

crate::utils::id_gen!(next_seat_id, SEAT_ID, SEAT_IDS);

#[repr(transparent)]
pub struct SeatId(pub usize);
pub struct ActiveOutput(pub RefCell<Output>);
#[derive(Default)]
pub struct SupressedKeys(RefCell<Vec<(u32, Option<RegistrationToken>)>>);
#[derive(Default)]
pub struct Devices(RefCell<HashMap<String, Vec<DeviceCapability>>>);

impl Default for SeatId {
    fn default() -> SeatId {
        SeatId(next_seat_id())
    }
}

impl Drop for SeatId {
    fn drop(&mut self) {
        SEAT_IDS.lock().unwrap().remove(&self.0);
    }
}

impl SupressedKeys {
    fn add(&self, keysym: &KeysymHandle, token: impl Into<Option<RegistrationToken>>) {
        self.0.borrow_mut().push((keysym.raw_code(), token.into()));
    }

    fn filter(&self, keysym: &KeysymHandle) -> Option<Vec<RegistrationToken>> {
        let mut keys = self.0.borrow_mut();
        let (removed, remaining) = keys
            .drain(..)
            .partition(|(key, _)| *key == keysym.raw_code());
        *keys = remaining;

        let removed = removed
            .into_iter()
            .map(|(_, token)| token)
            .flatten()
            .collect::<Vec<_>>();
        if removed.is_empty() {
            None
        } else {
            Some(removed)
        }
    }
}

impl Devices {
    fn add_device<D: Device>(&self, device: &D) -> Vec<DeviceCapability> {
        let id = device.id();
        let mut map = self.0.borrow_mut();
        let caps = [DeviceCapability::Keyboard, DeviceCapability::Pointer]
            .iter()
            .cloned()
            .filter(|c| device.has_capability(*c))
            .collect::<Vec<_>>();
        let new_caps = caps
            .iter()
            .cloned()
            .filter(|c| map.values().flatten().all(|has| *c != *has))
            .collect::<Vec<_>>();
        map.insert(id, caps);
        new_caps
    }

    pub fn has_device<D: Device>(&self, device: &D) -> bool {
        self.0.borrow().contains_key(&device.id())
    }

    fn remove_device<D: Device>(&self, device: &D) -> Vec<DeviceCapability> {
        let id = device.id();
        let mut map = self.0.borrow_mut();
        map.remove(&id)
            .unwrap_or(Vec::new())
            .into_iter()
            .filter(|c| map.values().flatten().all(|has| *c != *has))
            .collect()
    }
}

pub fn add_seat(
    dh: &DisplayHandle,
    seat_state: &mut SeatState<State>,
    output: &Output,
    config: &Config,
    name: String,
) -> Seat<State> {
    let mut seat = seat_state.new_wl_seat(dh, name);
    let userdata = seat.user_data();
    userdata.insert_if_missing(SeatId::default);
    userdata.insert_if_missing(Devices::default);
    userdata.insert_if_missing(SupressedKeys::default);
    userdata.insert_if_missing(SeatMoveGrabState::default);
    userdata.insert_if_missing(CursorState::default);
    userdata.insert_if_missing(|| ActiveOutput(RefCell::new(output.clone())));
    userdata.insert_if_missing(|| RefCell::new(CursorImageStatus::Default));

    // A lot of clients bind keyboard and pointer unconditionally once on launch..
    // Initial clients might race the compositor on adding periheral and
    // end up in a state, where they are not able to receive input.
    // Additionally a lot of clients don't handle keyboards/pointer objects being
    // removed very well either and we don't want to crash applications, because the
    // user is replugging their keyboard or mouse.
    //
    // So instead of doing the right thing (and initialize these capabilities as matching
    // devices appear), we have to surrender to reality and just always expose a keyboard and pointer.
    let conf = config.xkb_config();
    if let Err(err) = seat.add_keyboard(xkb_config_to_wl(&conf), 200, 25) {
        warn!(
            ?err,
            "Failed to load provided xkb config. Trying default...",
        );
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .expect("Failed to load xkb configuration files");
    }
    seat.add_pointer();

    seat
}

impl State {
    pub fn process_input_event<B: InputBackend>(
        &mut self,
        event: InputEvent<B>,
        needs_key_repetition: bool,
    ) where
        <B as InputBackend>::PointerAxisEvent: 'static,
    {
        use smithay::backend::input::Event;

        match event {
            InputEvent::DeviceAdded { device } => {
                let seat = &mut self.common.last_active_seat();
                let userdata = seat.user_data();
                let devices = userdata.get::<Devices>().unwrap();
                for cap in devices.add_device(&device) {
                    match cap {
                        // TODO: Handle touch, tablet
                        _ => {}
                    }
                }
                #[cfg(feature = "debug")]
                {
                    self.common.egui.state.handle_device_added(&device);
                }
            }
            InputEvent::DeviceRemoved { device } => {
                for seat in &mut self.common.seats() {
                    let userdata = seat.user_data();
                    let devices = userdata.get::<Devices>().unwrap();
                    if devices.has_device(&device) {
                        for cap in devices.remove_device(&device) {
                            match cap {
                                // TODO: Handle touch, tablet
                                _ => {}
                            }
                        }
                        break;
                    }
                }
                #[cfg(feature = "debug")]
                {
                    self.common.egui.state.handle_device_removed(&device);
                }
            }
            InputEvent::Keyboard { event, .. } => {
                use smithay::backend::input::KeyboardKeyEvent;

                let loop_handle = self.common.event_loop_handle.clone();

                if let Some(seat) = self.common.seat_with_device(&event.device()).cloned() {
                    let userdata = seat.user_data();

                    let current_output = seat.active_output();
                    let workspace = self.common.shell.active_space_mut(&current_output);
                    let shortcuts_inhibited = workspace
                        .focus_stack
                        .get(&seat)
                        .last()
                        .and_then(|window| {
                            window.wl_surface().and_then(|surface| {
                                seat.keyboard_shortcuts_inhibitor_for_surface(&surface)
                            })
                        })
                        .map(|inhibitor| inhibitor.is_active())
                        .unwrap_or(false);

                    let keycode = event.key_code();
                    let state = event.state();
                    trace!(?keycode, ?state, "key");

                    let serial = SERIAL_COUNTER.next_serial();
                    let time = Event::time_msec(&event);
                    let keyboard = seat.get_keyboard().unwrap();
                    let current_focus = keyboard.current_focus();
                    if let Some((action, pattern)) = keyboard
                            .input(
                                self,
                                keycode,
                                state,
                                serial,
                                time,
                                |data, modifiers, handle| {
                                    // Leave move overview mode, if any modifier was released
                                    if let OverviewMode::Started(Trigger::KeyboardMove(action_modifiers), _) =
                                        data.common.shell.overview_mode().0
                                    {
                                        if (action_modifiers.ctrl && !modifiers.ctrl)
                                            || (action_modifiers.alt && !modifiers.alt)
                                            || (action_modifiers.logo && !modifiers.logo)
                                            || (action_modifiers.shift && !modifiers.shift)
                                        {
                                            data.common.shell.set_overview_mode(None, data.common.event_loop_handle.clone());
                                        }
                                    }
                                    // Leave swap overview mode, if any key was released
                                    if let OverviewMode::Started(Trigger::KeyboardSwap(action_pattern, old_descriptor), _) =
                                        data.common.shell.overview_mode().0
                                    {
                                        if (action_pattern.modifiers.ctrl && !modifiers.ctrl)
                                            || (action_pattern.modifiers.alt && !modifiers.alt)
                                            || (action_pattern.modifiers.logo && !modifiers.logo)
                                            || (action_pattern.modifiers.shift && !modifiers.shift)
                                            || (handle.raw_syms().contains(&action_pattern.key) && state == KeyState::Released)
                                        {
                                            data.common.shell.set_overview_mode(None, data.common.event_loop_handle.clone());

                                            if let Some(focus) = current_focus {
                                                if let Some(new_descriptor) = data.common.shell.workspaces.active(&current_output).1.node_desc(focus) {
                                                    let mut spaces = data.common.shell.workspaces.spaces_mut();
                                                    if old_descriptor.handle != new_descriptor.handle {
                                                        let (mut old_w, mut other_w) = spaces.partition::<Vec<_>, _>(|w| w.handle == old_descriptor.handle);
                                                        if let Some(old_workspace) = old_w.get_mut(0) {
                                                            if let Some(new_workspace) = other_w.iter_mut().find(|w| w.handle == new_descriptor.handle) {
                                                                if let Some(focus) = TilingLayout::swap_trees(&mut old_workspace.tiling_layer, Some(&mut new_workspace.tiling_layer), &old_descriptor, &new_descriptor, &mut data.common.shell.toplevel_info_state) {
                                                                    let seat = seat.clone();
                                                                    data.common.event_loop_handle.insert_idle(move |data| {
                                                                        Common::set_focus(&mut data.state, Some(&focus), &seat, None);
                                                                    });
                                                                }
                                                                old_workspace.refresh_focus_stack();
                                                                new_workspace.refresh_focus_stack();
                                                            }
                                                        }
                                                    } else {
                                                        if let Some(workspace) = spaces.find(|w| w.handle == new_descriptor.handle) {
                                                            if let Some(focus) = TilingLayout::swap_trees(&mut workspace.tiling_layer, None, &old_descriptor, &new_descriptor, &mut data.common.shell.toplevel_info_state) {
                                                                std::mem::drop(spaces);
                                                                let seat = seat.clone();
                                                                data.common.event_loop_handle.insert_idle(move |data| {
                                                                    Common::set_focus(&mut data.state, Some(&focus), &seat, None);
                                                                });
                                                            }
                                                            workspace.refresh_focus_stack();
                                                        }
                                                    }
                                                }
                                            } else {
                                                let new_workspace = data.common.shell.workspaces.active(&current_output).1.handle;
                                                if new_workspace != old_descriptor.handle {
                                                    let spaces = data.common.shell.workspaces.spaces_mut();
                                                    let (mut old_w, mut other_w) = spaces.partition::<Vec<_>, _>(|w| w.handle == old_descriptor.handle);
                                                    if let Some(old_workspace) = old_w.get_mut(0) {
                                                        if let Some(new_workspace) = other_w.iter_mut().find(|w| w.handle == new_workspace) {
                                                            if new_workspace.tiling_layer.windows().next().is_none() {
                                                                if let Some(focus) = TilingLayout::move_tree(&mut old_workspace.tiling_layer, &mut new_workspace.tiling_layer, &current_output, &new_workspace.handle, &seat, new_workspace.focus_stack.get(&seat).iter(), old_descriptor, &mut data.common.shell.toplevel_info_state) {
                                                                    let seat = seat.clone();
                                                                    data.common.event_loop_handle.insert_idle(move |data| {
                                                                        Common::set_focus(&mut data.state, Some(&focus), &seat, None);
                                                                    });
                                                                }
                                                                old_workspace.refresh_focus_stack();
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Leave or update resize mode, if modifiers changed or initial key was released
                                    if let (ResizeMode::Started(action_pattern, _, _), _) =
                                        data.common.shell.resize_mode()
                                    {
                                        if state == KeyState::Released
                                            && handle.raw_syms().contains(&action_pattern.key)
                                        {
                                            data.common.shell.set_resize_mode(None, &data.common.config, data.common.event_loop_handle.clone());
                                        } else if action_pattern.modifiers != *modifiers {
                                            let mut new_pattern = action_pattern.clone();
                                            new_pattern.modifiers = modifiers.clone().into();
                                            let enabled = data
                                                .common
                                                .config
                                                .static_conf
                                                .key_bindings
                                                .iter()
                                                .find_map(move |(binding, action)| {
                                                    if binding == &new_pattern
                                                        && matches!(action, Action::Resizing(_))
                                                    {
                                                        let Action::Resizing(direction) = action else { unreachable!() };
                                                        Some((new_pattern.clone(), *direction))
                                                    } else {
                                                        None
                                                    }
                                                });
                                            data.common.shell.set_resize_mode(enabled, &data.common.config, data.common.event_loop_handle.clone());
                                        }
                                    }

                                    // Special case resizing with regards to arrow keys
                                    if let (ResizeMode::Started(_, _, direction), _) =
                                        data.common.shell.resize_mode()
                                    {
                                        let resize_edge = match handle.modified_sym() {
                                            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_H => Some(ResizeEdge::LEFT),
                                            keysyms::KEY_Down | keysyms::KEY_j | keysyms::KEY_J => Some(ResizeEdge::BOTTOM),
                                            keysyms::KEY_Up | keysyms::KEY_k | keysyms::KEY_K => Some(ResizeEdge::TOP),
                                            keysyms::KEY_Right | keysyms::KEY_l | keysyms::KEY_L => Some(ResizeEdge::RIGHT),
                                            _ => None,
                                        };

                                        if let Some(mut edge) = resize_edge {
                                            if direction == ResizeDirection::Inwards {
                                                edge.flip_direction();
                                            }
                                            let action = Action::_ResizingInternal(direction, edge, state);
                                            let key_pattern = KeyPattern {
                                                modifiers: modifiers.clone().into(),
                                                key: handle.raw_code(),
                                            };

                                            if state == KeyState::Released {
                                                if let Some(tokens) = userdata.get::<SupressedKeys>().unwrap().filter(&handle) {
                                                    for token in tokens {
                                                        loop_handle.remove(token);
                                                    }
                                                }
                                            } else {
                                                let token = if needs_key_repetition {
                                                    let seat_clone = seat.clone();
                                                    let action_clone = action.clone();
                                                    let key_pattern_clone = key_pattern.clone();
                                                    let start = Instant::now();
                                                    loop_handle.insert_source(Timer::from_duration(Duration::from_millis(200)), move |current, _, data| {
                                                        let duration = current.duration_since(start).as_millis();
                                                        data.state.handle_action(action_clone.clone(), &seat_clone, serial, time.overflowing_add(duration as u32).0, key_pattern_clone.clone(), None);
                                                        calloop::timer::TimeoutAction::ToDuration(Duration::from_millis(25))
                                                    }).ok()
                                                } else { None };

                                               userdata
                                                        .get::<SupressedKeys>()
                                                        .unwrap()
                                                        .add(&handle, token);
                                            }
                                            return FilterResult::Intercept(Some((
                                                action,
                                                key_pattern
                                            )));
                                        }
                                    }

                                    // Skip released events for initially surpressed keys
                                    if state == KeyState::Released {
                                        if let Some(tokens) = userdata.get::<SupressedKeys>().unwrap().filter(&handle) {
                                            for token in tokens {
                                                loop_handle.remove(token);
                                            }
                                            return FilterResult::Intercept(None);
                                        }
                                    }

                                    // Pass keys to debug interface, if it has focus
                                    #[cfg(feature = "debug")]
                                    {
                                        if data.common.seats().position(|x| x == &seat).unwrap() == 0
                                            && data.common.egui.active
                                        {
                                            if data.common.egui.state.wants_keyboard() {
                                                data.common.egui.state.handle_keyboard(
                                                    &handle,
                                                    state == KeyState::Pressed,
                                                    modifiers.clone(),
                                                );
                                                userdata
                                                    .get::<SupressedKeys>()
                                                    .unwrap()
                                                    .add(&handle, None);
                                                return FilterResult::Intercept(None);
                                            }
                                        }
                                    }

                                    // Handle VT switches
                                    if state == KeyState::Pressed
                                        && (keysyms::KEY_XF86Switch_VT_1..=KEY_XF86Switch_VT_12)
                                            .contains(&handle.modified_sym())
                                    {
                                        if let Err(err) = data.backend.kms().switch_vt(
                                            (handle.modified_sym() - keysyms::KEY_XF86Switch_VT_1
                                                + 1)
                                                as i32,
                                        ) {
                                            error!(?err, "Failed switching virtual terminal.");
                                        }
                                        userdata.get::<SupressedKeys>().unwrap().add(&handle, None);
                                        return FilterResult::Intercept(None);
                                    }

                                    // handle the rest of the global shortcuts
                                    if !shortcuts_inhibited {
                                        for (binding, action) in
                                            data.common.config.static_conf.key_bindings.iter()
                                        {
                                            if state == KeyState::Pressed
                                                && binding.modifiers == *modifiers
                                                && handle.raw_syms().contains(&binding.key)
                                            {
                                                userdata
                                                    .get::<SupressedKeys>()
                                                    .unwrap()
                                                    .add(&handle, None);
                                                return FilterResult::Intercept(Some((
                                                    action.clone(),
                                                    binding.clone(),
                                                )));
                                            }
                                        }
                                    }

                                    // keys are passed through to apps
                                    FilterResult::Forward
                                },
                            )
                            .flatten()
                        {
                            self.handle_action(action, &seat, serial, time, pattern, None)
                        }
                }
            }
            InputEvent::PointerMotion { event, .. } => {
                use smithay::backend::input::PointerMotionEvent;

                if let Some(seat) = self.common.seat_with_device(&event.device()).cloned() {
                    let current_output = seat.active_output();

                    let mut position = seat.get_pointer().unwrap().current_location();
                    position += event.delta();

                    let output = self
                        .common
                        .shell
                        .outputs()
                        .find(|output| output.geometry().to_f64().contains(position))
                        .cloned()
                        .unwrap_or(current_output.clone());
                    if output != current_output {
                        for session in sessions_for_output(&self.common, &current_output) {
                            session.cursor_leave(&seat, InputType::Pointer);
                        }

                        for session in sessions_for_output(&self.common, &output) {
                            session.cursor_enter(&seat, InputType::Pointer);
                        }

                        seat.set_active_output(&output);
                    }
                    let output_geometry = output.geometry();

                    position.x = (output_geometry.loc.x as f64)
                        .max(position.x)
                        .min((output_geometry.loc.x + output_geometry.size.w) as f64);
                    position.y = (output_geometry.loc.y as f64)
                        .max(position.y)
                        .min((output_geometry.loc.y + output_geometry.size.h) as f64);

                    let serial = SERIAL_COUNTER.next_serial();
                    let relative_pos = self.common.shell.map_global_to_space(position, &output);
                    let overview = self.common.shell.overview_mode();
                    let workspace = self.common.shell.workspaces.active_mut(&output);
                    let under = State::surface_under(
                        position,
                        relative_pos,
                        &output,
                        output_geometry,
                        &self.common.shell.override_redirect_windows,
                        overview.0,
                        workspace,
                    );

                    for session in sessions_for_output(&self.common, &output) {
                        if let Some((geometry, offset)) = seat.cursor_geometry(
                            position.to_buffer(
                                output.current_scale().fractional_scale(),
                                output.current_transform(),
                                &output.geometry().size.to_f64(),
                            ),
                            self.common.clock.now(),
                        ) {
                            session.cursor_info(&seat, InputType::Pointer, geometry, offset);
                        }
                    }
                    let ptr = seat.get_pointer().unwrap();
                    // Relative motion is sent first to ensure they're part of a `frame`
                    // TODO: Find more correct solution
                    ptr.relative_motion(
                        self,
                        under.clone(),
                        &RelativeMotionEvent {
                            delta: event.delta(),
                            delta_unaccel: event.delta_unaccel(),
                            utime: event.time(),
                        },
                    );
                    ptr.motion(
                        self,
                        under,
                        &MotionEvent {
                            location: position,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    #[cfg(feature = "debug")]
                    if self.common.seats().position(|x| x == &seat).unwrap() == 0 {
                        let location = if let Some(output) = self.common.shell.outputs.first() {
                            self.common
                                .shell
                                .map_global_to_space(position, output)
                                .to_i32_round()
                        } else {
                            position.to_i32_round()
                        };
                        self.common.egui.state.handle_pointer_motion(location);
                    }
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()).cloned() {
                    let output = seat.active_output();
                    let geometry = output.geometry();
                    let position = geometry.loc.to_f64()
                        + smithay::backend::input::AbsolutePositionEvent::position_transformed(
                            &event,
                            geometry.size,
                        );
                    let relative_pos = self.common.shell.map_global_to_space(position, &output);
                    let overview = self.common.shell.overview_mode();
                    let workspace = self.common.shell.workspaces.active_mut(&output);
                    let serial = SERIAL_COUNTER.next_serial();
                    let under = State::surface_under(
                        position,
                        relative_pos,
                        &output,
                        geometry,
                        &self.common.shell.override_redirect_windows,
                        overview.0,
                        workspace,
                    );

                    for session in sessions_for_output(&self.common, &output) {
                        if let Some((geometry, offset)) = seat.cursor_geometry(
                            position.to_buffer(
                                output.current_scale().fractional_scale(),
                                output.current_transform(),
                                &output.geometry().size.to_f64(),
                            ),
                            self.common.clock.now(),
                        ) {
                            session.cursor_info(&seat, InputType::Pointer, geometry, offset);
                        }
                    }
                    seat.get_pointer().unwrap().motion(
                        self,
                        under,
                        &MotionEvent {
                            location: position,
                            serial,
                            time: event.time_msec(),
                        },
                    );
                    #[cfg(feature = "debug")]
                    if self.common.seats().position(|x| x == &seat).unwrap() == 0 {
                        let location = if let Some(output) = self.common.shell.outputs.first() {
                            self.common
                                .shell
                                .map_global_to_space(position, output)
                                .to_i32_round()
                        } else {
                            position.to_i32_round()
                        };
                        self.common.egui.state.handle_pointer_motion(location);
                    }
                }
            }
            InputEvent::PointerButton { event, .. } => {
                use smithay::backend::input::{ButtonState, PointerButtonEvent};

                if let Some(seat) = self.common.seat_with_device(&event.device()).cloned() {
                    #[cfg(feature = "debug")]
                    if self.common.seats().position(|x| x == &seat).unwrap() == 0
                        && self.common.egui.active
                    {
                        if self.common.egui.state.wants_pointer() {
                            if let Some(button) = event.button() {
                                self.common.egui.state.handle_pointer_button(
                                    button,
                                    event.state() == ButtonState::Pressed,
                                );
                            }
                            return;
                        }
                    }

                    let serial = SERIAL_COUNTER.next_serial();
                    let button = event.button_code();
                    if event.state() == ButtonState::Pressed {
                        // change the keyboard focus unless the pointer or keyboard is grabbed
                        // We test for any matching surface type here but always use the root
                        // (in case of a window the toplevel) surface for the focus.
                        // see: https://gitlab.freedesktop.org/wayland/wayland/-/issues/294
                        if !seat.get_pointer().unwrap().is_grabbed()
                            && !seat.get_keyboard().map(|k| k.is_grabbed()).unwrap_or(false)
                        {
                            let output = seat.active_output();
                            let pos = seat.get_pointer().unwrap().current_location();
                            let relative_pos = self.common.shell.map_global_to_space(pos, &output);
                            let overview = self.common.shell.overview_mode();
                            let workspace = self.common.shell.active_space_mut(&output);
                            let mut under = None;

                            if let Some(window) = workspace.get_fullscreen(&output) {
                                let layers = layer_map_for_output(&output);
                                if let Some(layer) =
                                    layers.layer_under(WlrLayer::Overlay, relative_pos)
                                {
                                    let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                                    if layer.can_receive_keyboard_focus()
                                        && layer
                                            .surface_under(
                                                relative_pos - layer_loc.to_f64(),
                                                WindowSurfaceType::ALL,
                                            )
                                            .is_some()
                                    {
                                        under = Some(layer.clone().into());
                                    }
                                } else {
                                    under = Some(window.clone().into());
                                }
                            } else {
                                let done = {
                                    let layers = layer_map_for_output(&output);
                                    if let Some(layer) = layers
                                        .layer_under(WlrLayer::Overlay, relative_pos)
                                        .or_else(|| layers.layer_under(WlrLayer::Top, relative_pos))
                                    {
                                        let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                                        if layer.can_receive_keyboard_focus()
                                            && layer
                                                .surface_under(
                                                    relative_pos - layer_loc.to_f64(),
                                                    WindowSurfaceType::ALL,
                                                )
                                                .is_some()
                                        {
                                            under = Some(layer.clone().into());
                                        }
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if !done {
                                    if let Some(surface) = workspace.get_maximized(&output) {
                                        under = Some(surface.clone().into());
                                    } else {
                                        if let Some((target, _)) =
                                            workspace.element_under(relative_pos, overview.0)
                                        {
                                            under = Some(target);
                                        } else {
                                            let layers = layer_map_for_output(&output);
                                            if let Some(layer) = layers
                                                .layer_under(WlrLayer::Bottom, pos)
                                                .or_else(|| {
                                                    layers.layer_under(WlrLayer::Background, pos)
                                                })
                                            {
                                                let layer_loc =
                                                    layers.layer_geometry(layer).unwrap().loc;
                                                if layer.can_receive_keyboard_focus()
                                                    && layer
                                                        .surface_under(
                                                            relative_pos - layer_loc.to_f64(),
                                                            WindowSurfaceType::ALL,
                                                        )
                                                        .is_some()
                                                {
                                                    under = Some(layer.clone().into());
                                                }
                                            };
                                        }
                                    }
                                }
                            }
                            Common::set_focus(
                                self,
                                under.and_then(|target| target.try_into().ok()).as_ref(),
                                &seat,
                                Some(serial),
                            );
                        }
                    } else {
                        if let OverviewMode::Started(Trigger::Pointer(action_button), _) =
                            self.common.shell.overview_mode().0
                        {
                            if action_button == button {
                                self.common
                                    .shell
                                    .set_overview_mode(None, self.common.event_loop_handle.clone());
                            }
                        }
                    };
                    seat.get_pointer().unwrap().button(
                        self,
                        &ButtonEvent {
                            button,
                            state: event.state(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                }
            }
            InputEvent::PointerAxis { event, .. } => {
                #[allow(deprecated)]
                let scroll_factor = if let Some(event) =
                    <dyn Any>::downcast_ref::<LibinputPointerAxisEvent>(&event)
                {
                    self.common.config.scroll_factor(&event.device())
                } else {
                    1.0
                };

                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    #[cfg(feature = "debug")]
                    if self.common.seats().position(|x| x == seat).unwrap() == 0
                        && self.common.egui.active
                    {
                        if self.common.egui.state.wants_pointer() {
                            self.common.egui.state.handle_pointer_axis(
                                event
                                    .amount_discrete(Axis::Horizontal)
                                    .or_else(|| event.amount(Axis::Horizontal).map(|x| x * 3.0))
                                    .unwrap_or(0.0),
                                event
                                    .amount_discrete(Axis::Vertical)
                                    .or_else(|| event.amount(Axis::Vertical).map(|x| x * 3.0))
                                    .unwrap_or(0.0),
                            );
                            return;
                        }
                    }

                    let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                        event.amount_discrete(Axis::Horizontal).unwrap_or(0.0) * 3.0
                    });
                    let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                        event.amount_discrete(Axis::Vertical).unwrap_or(0.0) * 3.0
                    });
                    let horizontal_amount_discrete = event.amount_discrete(Axis::Horizontal);
                    let vertical_amount_discrete = event.amount_discrete(Axis::Vertical);

                    {
                        let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
                        if horizontal_amount != 0.0 {
                            frame =
                                frame.value(Axis::Horizontal, scroll_factor * horizontal_amount);
                            if let Some(discrete) = horizontal_amount_discrete {
                                frame = frame.discrete(Axis::Horizontal, discrete as i32);
                            }
                        } else if event.source() == AxisSource::Finger {
                            frame = frame.stop(Axis::Horizontal);
                        }
                        if vertical_amount != 0.0 {
                            frame = frame.value(Axis::Vertical, scroll_factor * vertical_amount);
                            if let Some(discrete) = vertical_amount_discrete {
                                frame = frame.discrete(Axis::Vertical, discrete as i32);
                            }
                        } else if event.source() == AxisSource::Finger {
                            frame = frame.stop(Axis::Vertical);
                        }
                        seat.get_pointer().unwrap().axis(self, frame);
                    }
                }
            }
            InputEvent::GestureSwipeBegin { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_swipe_begin(
                        self,
                        &GestureSwipeBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeUpdate { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_swipe_update(
                        self,
                        &GestureSwipeUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                        },
                    );
                }
            }
            InputEvent::GestureSwipeEnd { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_swipe_end(
                        self,
                        &GestureSwipeEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchBegin { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_pinch_begin(
                        self,
                        &GesturePinchBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchUpdate { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_pinch_update(
                        self,
                        &GesturePinchUpdateEvent {
                            time: event.time_msec(),
                            delta: event.delta(),
                            scale: event.scale(),
                            rotation: event.rotation(),
                        },
                    );
                }
            }
            InputEvent::GesturePinchEnd { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_pinch_end(
                        self,
                        &GesturePinchEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            InputEvent::GestureHoldBegin { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_hold_begin(
                        self,
                        &GestureHoldBeginEvent {
                            serial,
                            time: event.time_msec(),
                            fingers: event.fingers(),
                        },
                    );
                }
            }
            InputEvent::GestureHoldEnd { event, .. } => {
                if let Some(seat) = self.common.seat_with_device(&event.device()) {
                    let serial = SERIAL_COUNTER.next_serial();
                    let pointer = seat.get_pointer().unwrap();
                    pointer.gesture_hold_end(
                        self,
                        &GestureHoldEndEvent {
                            serial,
                            time: event.time_msec(),
                            cancelled: event.cancelled(),
                        },
                    );
                }
            }
            _ => { /* TODO e.g. tablet or touch events */ }
        }
    }

    pub fn handle_action(
        &mut self,
        action: Action,
        seat: &Seat<State>,
        serial: Serial,
        time: u32,
        pattern: KeyPattern,
        direction: Option<Direction>,
    ) {
        match action {
            Action::Terminate => {
                self.common.should_stop = true;
            }
            #[cfg(feature = "debug")]
            Action::Debug => {
                self.common.egui.active = !self.common.egui.active;
                puffin::set_scopes_on(self.common.egui.active);
                for mapped in self
                    .common
                    .shell
                    .workspaces
                    .spaces()
                    .flat_map(|w| w.mapped())
                {
                    mapped.set_debug(self.common.egui.active);
                }
            }
            #[cfg(not(feature = "debug"))]
            Action::Debug => {
                info!("Debug overlay not included in this build.")
            }
            Action::Close => {
                let current_output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&current_output);
                if let Some(window) = workspace.focus_stack.get(seat).last() {
                    window.send_close();
                }
            }
            Action::Workspace(key_num) => {
                let current_output = seat.active_output();
                let workspace = match key_num {
                    0 => 9,
                    x => x - 1,
                };
                let _ = self
                    .common
                    .shell
                    .activate(&current_output, workspace as usize);
            }
            Action::NextWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .active_num(&current_output)
                    .1
                    .saturating_add(1);
                if self
                    .common
                    .shell
                    .activate(&current_output, workspace)
                    .is_err()
                {
                    self.handle_action(Action::NextOutput, seat, serial, time, pattern, direction);
                }
            }
            Action::PreviousWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .active_num(&current_output)
                    .1
                    .saturating_sub(1);
                if self
                    .common
                    .shell
                    .activate(&current_output, workspace)
                    .is_err()
                {
                    self.handle_action(
                        Action::PreviousOutput,
                        seat,
                        serial,
                        time,
                        pattern,
                        direction,
                    );
                }
            }
            Action::LastWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .len(&current_output)
                    .saturating_sub(1);
                let _ = self.common.shell.activate(&current_output, workspace);
            }
            x @ Action::MoveToWorkspace(_) | x @ Action::SendToWorkspace(_) => {
                let current_output = seat.active_output();
                let follow = matches!(x, Action::MoveToWorkspace(_));
                let workspace = match x {
                    Action::MoveToWorkspace(0) | Action::SendToWorkspace(0) => 9,
                    Action::MoveToWorkspace(x) | Action::SendToWorkspace(x) => x - 1,
                    _ => unreachable!(),
                };
                let _ = Shell::move_current_window(
                    self,
                    seat,
                    &current_output,
                    (&current_output, Some(workspace as usize)),
                    follow,
                    None,
                );
            }
            x @ Action::MoveToNextWorkspace | x @ Action::SendToNextWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .active_num(&current_output)
                    .1
                    .saturating_add(1);
                if Shell::move_current_window(
                    self,
                    seat,
                    &current_output,
                    (&current_output, Some(workspace as usize)),
                    matches!(x, Action::MoveToNextWorkspace),
                    direction,
                )
                .is_err()
                {
                    self.handle_action(
                        if matches!(x, Action::MoveToNextWorkspace) {
                            Action::MoveToNextOutput
                        } else {
                            Action::SendToNextOutput
                        },
                        seat,
                        serial,
                        time,
                        pattern,
                        direction,
                    )
                }
            }
            x @ Action::MoveToPreviousWorkspace | x @ Action::SendToPreviousWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .active_num(&current_output)
                    .1
                    .saturating_sub(1);
                // TODO: Possibly move to prev output, if idx < 0
                if Shell::move_current_window(
                    self,
                    seat,
                    &current_output,
                    (&current_output, Some(workspace as usize)),
                    matches!(x, Action::MoveToPreviousWorkspace),
                    direction,
                )
                .is_err()
                {
                    self.handle_action(
                        if matches!(x, Action::MoveToNextWorkspace) {
                            Action::MoveToPreviousOutput
                        } else {
                            Action::SendToPreviousOutput
                        },
                        seat,
                        serial,
                        time,
                        pattern,
                        direction,
                    )
                }
            }
            x @ Action::MoveToLastWorkspace | x @ Action::SendToLastWorkspace => {
                let current_output = seat.active_output();
                let workspace = self
                    .common
                    .shell
                    .workspaces
                    .len(&current_output)
                    .saturating_sub(1);
                let _ = Shell::move_current_window(
                    self,
                    seat,
                    &current_output,
                    (&current_output, Some(workspace as usize)),
                    matches!(x, Action::MoveToLastWorkspace),
                    None,
                );
            }
            Action::NextOutput => {
                let current_output = seat.active_output();
                if let Some(next_output) = self
                    .common
                    .shell
                    .outputs
                    .iter()
                    .skip_while(|o| *o != &current_output)
                    .skip(1)
                    .next()
                    .cloned()
                {
                    let idx = self.common.shell.workspaces.active_num(&next_output).1;
                    match self.common.shell.activate(&next_output, idx) {
                        Ok(Some(new_pos)) => {
                            seat.set_active_output(&next_output);
                            if let Some(ptr) = seat.get_pointer() {
                                ptr.motion(
                                    self,
                                    None,
                                    &MotionEvent {
                                        location: new_pos.to_f64(),
                                        serial,
                                        time,
                                    },
                                );
                            }
                        }
                        Ok(None) => {
                            seat.set_active_output(&next_output);
                        }
                        _ => {}
                    }
                }
            }
            Action::PreviousOutput => {
                let current_output = seat.active_output();
                if let Some(prev_output) = self
                    .common
                    .shell
                    .outputs
                    .iter()
                    .rev()
                    .skip_while(|o| *o != &current_output)
                    .skip(1)
                    .next()
                    .cloned()
                {
                    let idx = self.common.shell.workspaces.active_num(&prev_output).1;
                    match self.common.shell.activate(&prev_output, idx) {
                        Ok(Some(new_pos)) => {
                            seat.set_active_output(&prev_output);
                            if let Some(ptr) = seat.get_pointer() {
                                ptr.motion(
                                    self,
                                    None,
                                    &MotionEvent {
                                        location: new_pos.to_f64(),
                                        serial,
                                        time,
                                    },
                                );
                            }
                        }
                        Ok(None) => {
                            seat.set_active_output(&prev_output);
                        }
                        _ => {}
                    }
                }
            }
            x @ Action::MoveToNextOutput | x @ Action::SendToNextOutput => {
                let current_output = seat.active_output();
                if let Some(next_output) = self
                    .common
                    .shell
                    .outputs
                    .iter()
                    .skip_while(|o| *o != &current_output)
                    .skip(1)
                    .next()
                    .cloned()
                {
                    if let Ok(Some(new_pos)) = Shell::move_current_window(
                        self,
                        seat,
                        &current_output,
                        (&next_output, None),
                        matches!(x, Action::MoveToNextOutput),
                        direction,
                    ) {
                        if let Some(ptr) = seat.get_pointer() {
                            ptr.motion(
                                self,
                                None,
                                &MotionEvent {
                                    location: new_pos.to_f64(),
                                    serial,
                                    time,
                                },
                            );
                        }
                    }
                }
            }
            x @ Action::MoveToPreviousOutput | x @ Action::SendToPreviousOutput => {
                let current_output = seat.active_output();
                if let Some(prev_output) = self
                    .common
                    .shell
                    .outputs
                    .iter()
                    .rev()
                    .skip_while(|o| *o != &current_output)
                    .skip(1)
                    .next()
                    .cloned()
                {
                    if let Ok(Some(new_pos)) = Shell::move_current_window(
                        self,
                        seat,
                        &current_output,
                        (&prev_output, None),
                        matches!(x, Action::MoveToPreviousOutput),
                        direction,
                    ) {
                        if let Some(ptr) = seat.get_pointer() {
                            ptr.motion(
                                self,
                                None,
                                &MotionEvent {
                                    location: new_pos.to_f64(),
                                    serial,
                                    time,
                                },
                            );
                        }
                    }
                }
            }
            Action::Focus(focus) => {
                let current_output = seat.active_output();
                let overview = self.common.shell.overview_mode().0;
                let workspace = self.common.shell.active_space_mut(&current_output);
                let result = workspace.next_focus(
                    focus,
                    seat,
                    match overview {
                        OverviewMode::Started(Trigger::KeyboardSwap(_, desc), _) => Some(desc),
                        _ => None,
                    },
                );

                match result {
                    FocusResult::None => {
                        match (focus, self.common.config.static_conf.workspace_layout) {
                            (FocusDirection::Left, WorkspaceLayout::Horizontal)
                            | (FocusDirection::Up, WorkspaceLayout::Vertical) => self
                                .handle_action(
                                    Action::PreviousWorkspace,
                                    seat,
                                    serial,
                                    time,
                                    pattern,
                                    direction,
                                ),
                            (FocusDirection::Right, WorkspaceLayout::Horizontal)
                            | (FocusDirection::Down, WorkspaceLayout::Vertical) => self
                                .handle_action(
                                    Action::NextWorkspace,
                                    seat,
                                    serial,
                                    time,
                                    pattern,
                                    direction,
                                ),
                            (FocusDirection::Left, WorkspaceLayout::Vertical)
                            | (FocusDirection::Up, WorkspaceLayout::Horizontal) => self
                                .handle_action(
                                    Action::PreviousOutput,
                                    seat,
                                    serial,
                                    time,
                                    pattern,
                                    direction,
                                ),
                            (FocusDirection::Right, WorkspaceLayout::Vertical)
                            | (FocusDirection::Down, WorkspaceLayout::Horizontal) => self
                                .handle_action(
                                    Action::NextOutput,
                                    seat,
                                    serial,
                                    time,
                                    pattern,
                                    direction,
                                ),
                            _ => {}
                        }
                    }
                    FocusResult::Handled => {}
                    FocusResult::Some(target) => {
                        Common::set_focus(self, Some(&target), seat, None);
                    }
                }
            }
            Action::Move(direction) => {
                let current_output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&current_output);
                if workspace.get_fullscreen(&current_output).is_some() {
                    return; // TODO, is this what we want? How do we indicate the switch?
                }

                match workspace.move_current_element(direction, seat) {
                    MoveResult::MoveFurther(_move_further) => {
                        match (direction, self.common.config.static_conf.workspace_layout) {
                            (Direction::Left, WorkspaceLayout::Horizontal)
                            | (Direction::Up, WorkspaceLayout::Vertical) => self.handle_action(
                                Action::MoveToPreviousWorkspace,
                                seat,
                                serial,
                                time,
                                pattern,
                                Some(direction),
                            ),
                            (Direction::Right, WorkspaceLayout::Horizontal)
                            | (Direction::Down, WorkspaceLayout::Vertical) => self.handle_action(
                                Action::MoveToNextWorkspace,
                                seat,
                                serial,
                                time,
                                pattern,
                                Some(direction),
                            ),
                            (Direction::Left, WorkspaceLayout::Vertical)
                            | (Direction::Up, WorkspaceLayout::Horizontal) => self.handle_action(
                                Action::MoveToPreviousOutput,
                                seat,
                                serial,
                                time,
                                pattern,
                                Some(direction),
                            ),
                            (Direction::Right, WorkspaceLayout::Vertical)
                            | (Direction::Down, WorkspaceLayout::Horizontal) => self.handle_action(
                                Action::MoveToNextOutput,
                                seat,
                                serial,
                                time,
                                pattern,
                                Some(direction),
                            ),
                        }
                    }
                    MoveResult::ShiftFocus(shift) => {
                        Common::set_focus(self, Some(&shift), seat, None);
                    }
                    _ => {
                        if let Some(focused_window) = workspace.focus_stack.get(seat).last() {
                            if workspace.is_tiled(focused_window) {
                                self.common.shell.set_overview_mode(
                                    Some(Trigger::KeyboardMove(pattern.modifiers)),
                                    self.common.event_loop_handle.clone(),
                                );
                            }
                        }
                    }
                }
            }
            Action::SwapWindow => {
                let current_output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&current_output);
                if workspace.get_fullscreen(&current_output).is_some() {
                    return; // TODO, is this what we want? Maybe disengage fullscreen instead?
                }

                let keyboard_handle = seat.get_keyboard().unwrap();
                if let Some(focus) = keyboard_handle.current_focus() {
                    if let Some(descriptor) = workspace.node_desc(focus) {
                        let grab = SwapWindowGrab::new(seat.clone(), descriptor.clone());
                        keyboard_handle.set_grab(grab, serial);
                        self.common.shell.set_overview_mode(
                            Some(Trigger::KeyboardSwap(pattern, descriptor)),
                            self.common.event_loop_handle.clone(),
                        );
                    }
                }
            }
            Action::Maximize => {
                let current_output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&current_output);
                let focus_stack = workspace.focus_stack.get(seat);
                let focused_window = focus_stack.last();
                if let Some(window) = focused_window.map(|f| f.active_window()) {
                    workspace.maximize_toggle(
                        &window,
                        &current_output,
                        self.common.event_loop_handle.clone(),
                    );
                }
            }
            Action::Resizing(direction) => self.common.shell.set_resize_mode(
                Some((pattern, direction)),
                &self.common.config,
                self.common.event_loop_handle.clone(),
            ),
            Action::_ResizingInternal(direction, edge, state) => {
                if state == KeyState::Pressed {
                    self.common.shell.resize(seat, direction, edge);
                } else {
                    self.common.shell.finish_resize(direction, edge);
                }
            }
            Action::ToggleOrientation => {
                let output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&output);
                workspace.tiling_layer.update_orientation(None, &seat);
            }
            Action::Orientation(orientation) => {
                let output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&output);
                workspace
                    .tiling_layer
                    .update_orientation(Some(orientation), &seat);
            }
            Action::ToggleStacking => {
                let output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&output);
                let focus_stack = workspace.focus_stack.get_mut(seat);
                workspace.tiling_layer.toggle_stacking(seat, focus_stack);
            }
            Action::ToggleTiling => {
                let output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&output);
                workspace.toggle_tiling(seat);
            }
            Action::ToggleWindowFloating => {
                let output = seat.active_output();
                let workspace = self.common.shell.active_space_mut(&output);
                workspace.toggle_floating_window(seat);
            }
            Action::Spawn(command) => {
                let wayland_display = self.common.socket.clone();

                let display = self
                    .common
                    .xwayland_state
                    .as_ref()
                    .map(|s| format!(":{}", s.display))
                    .unwrap_or_default();

                std::thread::spawn(move || {
                    let mut cmd = std::process::Command::new("/bin/sh");

                    cmd.arg("-c")
                        .arg(command.clone())
                        .env("WAYLAND_DISPLAY", &wayland_display)
                        .env("DISPLAY", &display)
                        .env_remove("COSMIC_SESSION_SOCK");

                    match cmd.spawn() {
                        Ok(mut child) => {
                            let _res = child.wait();
                        }
                        Err(err) => {
                            tracing::warn!(?err, "Failed to spawn \"{}\"", command);
                        }
                    }
                });
            }
        }
    }

    pub fn surface_under(
        global_pos: Point<f64, Logical>,
        relative_pos: Point<f64, Logical>,
        output: &Output,
        output_geo: Rectangle<i32, Logical>,
        override_redirect_windows: &[X11Surface],
        overview: OverviewMode,
        workspace: &mut Workspace,
    ) -> Option<(PointerFocusTarget, Point<i32, Logical>)> {
        if let Some(window) = workspace.get_fullscreen(output) {
            let layers = layer_map_for_output(output);
            if let Some(layer) = layers.layer_under(WlrLayer::Overlay, relative_pos) {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                if layer
                    .surface_under(relative_pos - layer_loc.to_f64(), WindowSurfaceType::ALL)
                    .is_some()
                {
                    return Some((layer.clone().into(), output_geo.loc + layer_loc));
                }
            }
            if let Some(or) = override_redirect_windows
                .iter()
                .find(|or| or.is_in_input_region(&(global_pos - or.geometry().loc.to_f64())))
            {
                return Some((or.clone().into(), or.geometry().loc));
            }
            Some((window.clone().into(), output_geo.loc))
        } else {
            {
                let layers = layer_map_for_output(output);
                if let Some(layer) = layers
                    .layer_under(WlrLayer::Overlay, relative_pos)
                    .or_else(|| layers.layer_under(WlrLayer::Top, relative_pos))
                {
                    let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                    if layer
                        .surface_under(relative_pos - layer_loc.to_f64(), WindowSurfaceType::ALL)
                        .is_some()
                    {
                        return Some((layer.clone().into(), output_geo.loc + layer_loc));
                    }
                }
            }
            if let Some(or) = override_redirect_windows
                .iter()
                .find(|or| or.is_in_input_region(&(global_pos - or.geometry().loc.to_f64())))
            {
                return Some((or.clone().into(), or.geometry().loc));
            }
            if let Some(surface) = workspace.get_maximized(output) {
                let offset = layer_map_for_output(output).non_exclusive_zone().loc;
                return Some((surface.clone().into(), output_geo.loc + offset));
            } else {
                if let Some((target, loc)) = workspace.element_under(relative_pos, overview) {
                    return Some((target, loc + (global_pos - relative_pos).to_i32_round()));
                }
                {
                    let layers = layer_map_for_output(output);
                    if let Some(layer) = layers
                        .layer_under(WlrLayer::Bottom, relative_pos)
                        .or_else(|| layers.layer_under(WlrLayer::Background, relative_pos))
                    {
                        let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                        if layer
                            .surface_under(
                                relative_pos - layer_loc.to_f64(),
                                WindowSurfaceType::ALL,
                            )
                            .is_some()
                        {
                            return Some((layer.clone().into(), output_geo.loc + layer_loc));
                        }
                    }
                }
            }
            None
        }
    }
}

fn sessions_for_output(state: &Common, output: &Output) -> impl Iterator<Item = Session> {
    let workspace = state.shell.active_space(&output);
    let maybe_fullscreen = workspace.get_fullscreen(&output);
    workspace
        .screencopy_sessions
        .iter()
        .map(|s| (&**s).clone())
        .chain(
            maybe_fullscreen
                .as_ref()
                .and_then(|w| {
                    if let Some(sessions) = w.surface().user_data().get::<ScreencopySessions>() {
                        Some(
                            sessions
                                .0
                                .borrow()
                                .iter()
                                .map(|s| (&**s).clone())
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        None
                    }
                })
                .into_iter()
                .flatten(),
        )
        .chain(
            output
                .user_data()
                .get::<ScreencopySessions>()
                .map(|sessions| {
                    sessions
                        .0
                        .borrow()
                        .iter()
                        .map(|s| (&**s).clone())
                        .collect::<Vec<_>>()
                })
                .into_iter()
                .into_iter()
                .flatten(),
        )
        .collect::<Vec<_>>()
        .into_iter()
}
