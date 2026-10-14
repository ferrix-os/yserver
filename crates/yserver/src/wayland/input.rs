//! The compositor's seat, into X: keys, the pointer, the wheel, the
//! keyboard focus and the cursor (Ferrix's `docs/YSERVER.md` §4.4, Y4).
//!
//! The compositor tells a client about its own windows only: which one the
//! pointer is on and where in it, which one has the keyboard. The server
//! turns that into what an X server that owns the screen would have seen.
//!
//! * **The pointer.** An X top-level keeps the root position X gave it,
//!   wherever the compositor shows it, so a position on the compositor's
//!   window is that window's X origin plus the surface-local position. The
//!   hit test is kept to the top-level the pointer is on
//!   ([`PointerScope`]), because two X windows the compositor shows apart
//!   can overlap in root coordinates; off every X window the pointer is on
//!   the root, which gives the last window its `LeaveNotify`.
//! * **Buttons** pass through as evdev codes. **The wheel** becomes X's
//!   buttons 4 to 7, one press and release a click: the compositor's clicks
//!   where it counts them, else ten units of its scroll a click, which is
//!   what a wheel click is in `wl_pointer.axis`.
//! * **Keys** are evdev codes plus 8, cooked by the server's own XKB state,
//!   which takes the compositor's layout when it connects
//!   ([`WaylandLink::adopt_keyboard_layout`]). The toolkit's own repeat is
//!   dropped: the X server repeats a held key itself, per
//!   `ChangeKeyboardControl`.
//! * **Focus.** The keyboard entering a window gives its X top-level the
//!   input focus, as a window manager would; leaving takes it away (focus
//!   None) and lets go of every key the server still has down.
//! * **The cursor** is the X cursor in effect, copied into a buffer and
//!   given with `wl_pointer.set_cursor` whenever it changes; with none, the
//!   compositor's arrow.
//! * **The input's time** (Ferrix's `docs/GAME-PATH.md` §4.7): where the
//!   compositor serves `zwp_relative_pointer_v1`, the server asks for one
//!   on the seat's pointer. Its `relative_motion` carries the time the
//!   input was stamped with when the device reported it, in microseconds of
//!   `CLOCK_MONOTONIC` (hyprix sends the input core's stamp), and the
//!   device's own distance. The pointer motion it comes before is handed to
//!   X with that time, as server milliseconds, and that distance as XI2's
//!   raw motion; with `YSERVER_INPUT_AGES=1` the server logs
//!   `input-age stamp <us> xtime <ms> age <us>` as it does, which the
//!   latency probe pairs with the compositor's and its own.

use std::collections::BTreeSet;

use compositor_toolkit::protocol::core::{WL_POINTER, wl_seat};
use compositor_toolkit::protocol::relative_pointer::{
    ZWP_RELATIVE_POINTER_MANAGER_V1, ZWP_RELATIVE_POINTER_V1, zwp_relative_pointer_manager_v1,
    zwp_relative_pointer_v1,
};
use compositor_toolkit::{
    Client, CursorShape, Event, KeyboardEvent, ObjectId, PointerEvent, SurfaceId, Value,
};
use yserver_core::{
    backend::{ActiveCursorImage, Backend},
    core_loop::{
        message::{
            HostInputEvent, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
            SYNTH_SCROLL_UP,
        },
        process_request::set_input_focus_for_server,
    },
    host_x11::HostKeyEvent,
    resources::ROOT_WINDOW,
    server::{PointerScope, ServerState},
};
use yserver_protocol::x11::ResourceId;

use super::WaylandLink;

/// How much `wl_pointer.axis` scroll is one wheel click, where the
/// compositor does not count clicks itself.
const SCROLL_PER_CLICK: f64 = 10.0;

/// The oldest a relative motion's time may be and still be taken as the
/// input's: a compositor whose `relative_motion` time is not
/// `CLOCK_MONOTONIC` gives a time that is far from now, and then the motion
/// is handed to X at the server's time instead.
const OLDEST_STAMP_US: u64 = 10_000_000;

/// What the renderer knows of the cursor in effect.
pub trait CursorSource {
    /// Which cursor is in effect, as the cursor's XID and a number that
    /// changes whenever its picture does; `None` for no cursor at all.
    fn cursor_key(&self) -> Option<(u32, u64)>;

    /// The cursor in effect.
    fn cursor_image(&self) -> Option<ActiveCursorImage>;
}

/// The seat's state between loop iterations.
#[derive(Debug, Default)]
pub(super) struct Seat {
    /// What the compositor said since the last [`WaylandLink::deliver_input`].
    queue: Vec<Event>,
    /// The compositor's window the pointer is on.
    pointer: Option<SurfaceId>,
    /// Where the pointer last was, in root coordinates.
    at: (i32, i32),
    /// Evdev buttons the server has down.
    buttons: BTreeSet<u32>,
    /// X keycodes the server has down.
    keys: BTreeSet<u8>,
    /// Scroll that is not yet a whole click: down, right.
    scroll: (f64, f64),
    /// The cursor last given to the compositor ([`CursorSource::cursor_key`]);
    /// the outer `None` before the first.
    cursor: Option<Option<(u32, u64)>>,
    /// The compositor's `zwp_relative_pointer_manager_v1`, where it serves
    /// one.
    relative_manager: Option<ObjectId>,
    /// The `zwp_relative_pointer_v1` on a pointer of the server's own, once
    /// asked for.
    relative: Option<ObjectId>,
    /// Whether the seat has had a pointer, which is when one may be asked
    /// for.
    pointed: bool,
    /// The relative motion since a pointer motion was last handed to X: the
    /// latest one's time in microseconds of `CLOCK_MONOTONIC`, and the
    /// distance summed.
    moved: Option<(u64, f64, f64)>,
    /// Whether each motion's age is logged (`YSERVER_INPUT_AGES=1`).
    ages: bool,
}

impl Seat {
    /// Keep a seat event for [`WaylandLink::deliver_input`]; anything else
    /// back to the caller.
    pub(super) fn take(&mut self, event: Event) -> Option<Event> {
        match event {
            Event::Keyboard(_) => {
                self.queue.push(event);
                None
            }
            Event::Pointer(_) => {
                self.pointed = true;
                self.queue.push(event);
                None
            }
            // In order with the pointer's own events: a relative motion
            // comes before the pointer motion it belongs to.
            Event::Object { object, .. } if Some(object) == self.relative => {
                self.queue.push(event);
                None
            }
            other => Some(other),
        }
    }

    /// Bind the compositor's relative pointer manager, where it has one,
    /// and read whether ages are to be logged.
    pub(super) fn bind_relative(&mut self, client: &mut Client) {
        self.relative_manager = client
            .bind(&ZWP_RELATIVE_POINTER_MANAGER_V1, 1, None)
            .ok()
            .map(|(manager, _)| manager);
        if self.relative_manager.is_none() {
            log::info!("wayland: no zwp_relative_pointer_manager_v1; input is stamped on arrival");
        }
        self.ages = std::env::var("YSERVER_INPUT_AGES").is_ok_and(|value| value == "1");
    }

    /// Ask for the relative pointer once the seat has a pointer: on a
    /// `wl_pointer` of the server's own, since the client runtime keeps its
    /// own to itself. The compositor sends relative motion to every relative
    /// pointer of the client the pointer is over, whichever `wl_pointer` it
    /// was made on; the second pointer's own events are not read.
    pub(super) fn ask_relative(&mut self, client: &mut Client) {
        let (None, true, Some(manager), Some(seat)) = (
            self.relative,
            self.pointed,
            self.relative_manager,
            client.seat(),
        ) else {
            return;
        };
        let pointer = client.new_object(&WL_POINTER, 1);
        let id = client.new_object(&ZWP_RELATIVE_POINTER_V1, 1);
        let asked = client
            .request(
                seat,
                wl_seat::request::GET_POINTER,
                &[Value::NewId(pointer)],
            )
            .and_then(|()| {
                client.request(
                    manager,
                    zwp_relative_pointer_manager_v1::request::GET_RELATIVE_POINTER,
                    &[Value::NewId(id), Value::Object(pointer)],
                )
            });
        match asked {
            Ok(()) => self.relative = Some(id),
            Err(error) => {
                log::warn!("wayland: the relative pointer: {error}");
                self.relative_manager = None;
            }
        }
    }

    /// One `relative_motion`: its time, and its distance before any
    /// acceleration, added to what is waiting for the next pointer motion.
    fn relative_motion(&mut self, opcode: u16, args: &[Value]) {
        if opcode != zwp_relative_pointer_v1::event::RELATIVE_MOTION {
            return;
        }
        let (Some(high), Some(low)) = (
            args.first().and_then(Value::as_uint),
            args.get(1).and_then(Value::as_uint),
        ) else {
            return;
        };
        let fixed = |at: usize| match args.get(at) {
            Some(Value::Fixed(value)) => *value,
            _ => 0.0,
        };
        let stamp = (u64::from(high) << 32) | u64::from(low);
        let (_, dx, dy) = self.moved.unwrap_or((0, 0.0, 0.0));
        self.moved = Some((stamp, dx + fixed(4), dy + fixed(5)));
    }
}

/// The X time of an input stamped `stamp` (microseconds of
/// `CLOCK_MONOTONIC`) when it is `now`: the server's time less the input's
/// age. `None` for a stamp from the future or more than
/// [`OLDEST_STAMP_US`] old, which is not this clock's.
fn x_time(stamp: u64, now: u64, server_ms: u32) -> Option<u32> {
    let age = now
        .checked_sub(stamp)
        .filter(|age| *age <= OLDEST_STAMP_US)?;
    Some(server_ms.wrapping_sub(u32::try_from(age / 1000).ok()?))
}

impl WaylandLink {
    /// Take the compositor's keyboard layout for the server's keymap, so a
    /// key means in X what it means on the compositor's other windows. Once,
    /// before any client connects: afterwards a new keymap would owe the
    /// clients a MappingNotify, and the compositor's layout is read only
    /// when it starts.
    pub(crate) fn adopt_keyboard_layout(&mut self, core: &mut crate::kms::core::KmsCore) {
        if self.client.keyboard_layout().is_none() {
            // The keymap comes after the seat's keyboard is made, which the
            // first round trips only start.
            match self.client.roundtrip() {
                Ok(events) => {
                    for event in events {
                        self.handle(event);
                    }
                }
                Err(error) => log::warn!("wayland: waiting for the keymap: {error}"),
            }
        }
        let Some((layout, _)) = self.client.keyboard_layout() else {
            log::info!(
                "wayland: the compositor sent no keymap; the keyboard stays {}",
                core.xkb_rmlvo.layout
            );
            return;
        };
        let rmlvo = crate::kms::core::XkbRmlvo {
            layout: layout.name.to_owned(),
            variant: layout.variant.to_owned(),
            ..core.xkb_rmlvo.clone()
        };
        if core.recompile_keymap(&rmlvo).is_some() {
            log::info!(
                "wayland: the keyboard is the compositor's, {} {:?}",
                layout.name,
                layout.variant
            );
        }
    }

    /// Hand what the compositor's seat did since the last call to X.
    pub fn deliver_input(&mut self, state: &mut ServerState, backend: &mut dyn Backend) {
        for event in std::mem::take(&mut self.seat.queue) {
            match event {
                Event::Pointer(event) => self.pointer_event(state, backend, event),
                Event::Keyboard(event) => self.keyboard_event(state, backend, event),
                Event::Object { opcode, args, .. } => self.seat.relative_motion(opcode, &args),
                _ => {}
            }
        }
    }

    /// The compositor's window the pointer is on, if any.
    pub(super) fn pointer_surface(&self) -> Option<SurfaceId> {
        self.seat.pointer
    }

    /// The X window the compositor's window or popup `surface` is, if it
    /// still is one.
    fn window_of(&self, state: &ServerState, surface: SurfaceId) -> Option<ResourceId> {
        let host_xid = self.host_xid_of(surface)?;
        state
            .resources
            .children(ROOT_WINDOW)
            .iter()
            .copied()
            .find(|&child| {
                state
                    .resources
                    .window(child)
                    .and_then(|window| window.host_xid)
                    .is_some_and(|xid| xid.as_raw() == host_xid)
            })
    }

    /// Where a position on the compositor's window `surface` is on the root.
    fn root_position(
        &self,
        state: &ServerState,
        surface: SurfaceId,
        x: f64,
        y: f64,
    ) -> Option<(i32, i32)> {
        let window = self.window_of(state, surface)?;
        let (left, top) = state.resources.window_absolute_position(window);
        Some(root_point((left, top), (x, y)))
    }

    fn pointer_event(
        &mut self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        event: PointerEvent,
    ) {
        match event {
            PointerEvent::Enter { surface, x, y } => {
                self.seat.pointer = Some(surface);
                state.pointer_scope = self
                    .window_of(state, surface)
                    .map_or(PointerScope::Nowhere, PointerScope::Within);
                if let Some(at) = self.root_position(state, surface, x, y) {
                    self.seat.at = at;
                }
                self.move_pointer(state, backend);
            }
            PointerEvent::Leave { .. } => {
                self.seat.pointer = None;
                for button in std::mem::take(&mut self.seat.buttons) {
                    press(state, backend, button, false);
                }
                state.pointer_scope = PointerScope::Nowhere;
                self.move_pointer(state, backend);
            }
            PointerEvent::Motion { surface, x, y } => {
                if let Some(at) = self.root_position(state, surface, x, y) {
                    self.seat.at = at;
                    self.move_pointer(state, backend);
                }
            }
            PointerEvent::Button {
                surface,
                x,
                y,
                button,
                pressed,
                ..
            } => {
                if let Some(at) = self.root_position(state, surface, x, y)
                    && at != self.seat.at
                {
                    self.seat.at = at;
                    self.move_pointer(state, backend);
                }
                let changed = if pressed {
                    self.seat.buttons.insert(button)
                } else {
                    self.seat.buttons.remove(&button)
                };
                if changed {
                    press(state, backend, button, pressed);
                }
            }
            PointerEvent::Axis {
                vertical,
                horizontal,
                discrete,
                ..
            } => {
                let down = clicks(&mut self.seat.scroll.0, vertical, discrete.0);
                let right = clicks(&mut self.seat.scroll.1, horizontal, discrete.1);
                for (count, forward, backward) in [
                    (down, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_UP),
                    (right, SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_LEFT),
                ] {
                    let button = if count > 0 { forward } else { backward };
                    for _ in 0..count.unsigned_abs() {
                        press(state, backend, u32::from(button), true);
                        press(state, backend, u32::from(button), false);
                    }
                }
            }
        }
    }

    /// Tell X the pointer is at [`Seat::at`], which also recomputes the
    /// window it is in and sends the crossings.
    ///
    /// With the relative motion that came before it, the motion is handed on
    /// with that motion's time and its distance (the module's comment).
    fn move_pointer(&mut self, state: &mut ServerState, backend: &mut dyn Backend) {
        let (x, y) = self.seat.at;
        let now = super::clock::monotonic_us();
        let server = crate::clock::server_time_ms();
        let stamped = self
            .seat
            .moved
            .take()
            .and_then(|(stamp, dx, dy)| Some((stamp, x_time(stamp, now, server)?, dx, dy)));
        #[allow(clippy::cast_possible_truncation)]
        let (time, dx, dy) = stamped.map_or((server, 0, 0), |(_, time, dx, dy)| {
            (time, dx.round() as i32, dy.round() as i32)
        });
        yserver_core::core_loop::handle_host_input(
            state,
            backend,
            HostInputEvent::PointerMotion {
                x,
                y,
                time,
                relative: false,
                dx,
                dy,
            },
        );
        if self.seat.ages
            && let Some((stamp, ..)) = stamped
        {
            log::info!(
                "input-age stamp {stamp} xtime {time} age {}",
                super::clock::monotonic_us().saturating_sub(stamp)
            );
        }
    }

    fn keyboard_event(
        &mut self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        event: KeyboardEvent,
    ) {
        match event {
            KeyboardEvent::Enter(surface) => {
                let Some(window) = self.window_of(state, surface) else {
                    return;
                };
                if set_input_focus_for_server(state, Some(window)) {
                    log::info!("wayland: the keyboard is on window 0x{:x}", window.0);
                }
            }
            KeyboardEvent::Leave(_) => {
                for keycode in std::mem::take(&mut self.seat.keys) {
                    self.key(state, backend, keycode, false);
                }
                let _ = set_input_focus_for_server(state, None);
            }
            KeyboardEvent::Key(key) => {
                // The server repeats a held key itself.
                if key.repeat {
                    return;
                }
                let Some(keycode) = x_keycode(key.code) else {
                    log::debug!("wayland: key {} has no X keycode", key.code);
                    return;
                };
                let changed = if key.pressed {
                    self.seat.keys.insert(keycode)
                } else {
                    self.seat.keys.remove(&keycode)
                };
                if changed {
                    self.key(state, backend, keycode, key.pressed);
                }
            }
            // The server's XKB state follows the keys themselves.
            KeyboardEvent::Modifiers(_) => {}
        }
    }

    fn key(&self, state: &mut ServerState, backend: &mut dyn Backend, keycode: u8, pressed: bool) {
        let (x, y) = self.seat.at;
        let x = i16::try_from(x).unwrap_or(i16::MAX);
        let y = i16::try_from(y).unwrap_or(i16::MAX);
        yserver_core::core_loop::handle_host_input(
            state,
            backend,
            HostInputEvent::Key(HostKeyEvent {
                pressed,
                keycode,
                time: crate::clock::server_time_ms(),
                root_x: x,
                root_y: y,
                event_x: x,
                event_y: y,
                state: 0,
            }),
        );
    }

    /// Give the compositor the X cursor in effect if it changed since it was
    /// last given. Once per loop iteration.
    pub fn sync_cursor(&mut self, cursors: &dyn CursorSource) {
        let key = cursors.cursor_key();
        if self.seat.cursor == Some(key) {
            return;
        }
        self.seat.cursor = Some(key);
        let Some(image) = key.and_then(|_| cursors.cursor_image()) else {
            self.client.set_cursor(CursorShape::default());
            return;
        };
        let pixels = premultiplied(&image.bgra_bytes);
        match self.client.set_cursor_image(
            u32::from(image.width),
            u32::from(image.height),
            (i32::from(image.hot_x), i32::from(image.hot_y)),
            &pixels,
        ) {
            Ok(()) => log::debug!(
                "wayland: the cursor is {}x{} at ({}, {})",
                image.width,
                image.height,
                image.hot_x,
                image.hot_y
            ),
            Err(error) => {
                log::warn!("wayland: the cursor: {error}");
                self.client.set_cursor(CursorShape::default());
            }
        }
    }
}

/// Press or let go of pointer button `button` (an evdev code, or one of
/// the server's synthetic scroll codes).
fn press(state: &mut ServerState, backend: &mut dyn Backend, button: u32, pressed: bool) {
    let Ok(button) = u16::try_from(button) else {
        return;
    };
    yserver_core::core_loop::handle_host_input(
        state,
        backend,
        HostInputEvent::PointerButton {
            button,
            pressed,
            time: crate::clock::server_time_ms(),
        },
    );
}

/// A surface-local position on a window whose X origin is `origin`, on
/// the root.
fn root_point(origin: (i32, i32), (x, y): (f64, f64)) -> (i32, i32) {
    #[allow(clippy::cast_possible_truncation)]
    let local = (x.floor() as i32, y.floor() as i32);
    (
        origin.0.saturating_add(local.0),
        origin.1.saturating_add(local.1),
    )
}

/// Whole wheel clicks from one axis event: the compositor's own count when
/// it gives one, which also forgets what was left over, else `value`
/// added to `residue`, a click every [`SCROLL_PER_CLICK`].
fn clicks(residue: &mut f64, value: f64, discrete: i32) -> i32 {
    if discrete != 0 {
        *residue = 0.0;
        return discrete;
    }
    *residue += value;
    let whole = (*residue / SCROLL_PER_CLICK).trunc();
    *residue -= whole * SCROLL_PER_CLICK;
    #[allow(clippy::cast_possible_truncation)]
    let whole = whole as i32;
    whole
}

/// An evdev key code as an X keycode, which is 8 more and at most 255.
fn x_keycode(code: u32) -> Option<u8> {
    u8::try_from(code.checked_add(8)?).ok()
}

/// A cursor's straight-alpha B, G, R, A pixels as `wl_shm`'s
/// premultiplied ARGB8888, which is B, G, R, A in memory too.
fn premultiplied(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        let alpha = u16::from(pixel[3]);
        let scale = |channel: u8| {
            #[allow(clippy::cast_possible_truncation)]
            let scaled = ((u16::from(channel) * alpha + 127) / 255) as u8;
            scaled
        };
        out.extend_from_slice(&[scale(pixel[0]), scale(pixel[1]), scale(pixel[2]), pixel[3]]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_on_a_window_is_its_origin_plus_the_local_one() {
        assert_eq!(root_point((100, 50), (10.7, 0.2)), (110, 50));
        assert_eq!(root_point((0, 0), (-0.5, 3.0)), (-1, 3));
    }

    #[test]
    fn the_wheel_counts_the_compositors_clicks_or_ten_units_a_click() {
        let mut residue = 0.0;
        assert_eq!(clicks(&mut residue, 15.0, 1), 1, "its own count");
        assert_eq!(residue, 0.0);
        assert_eq!(clicks(&mut residue, 4.0, 0), 0);
        assert_eq!(clicks(&mut residue, 7.0, 0), 1, "eleven units: a click");
        assert!((residue - 1.0).abs() < 1e-9);
        assert_eq!(clicks(&mut residue, -21.0, 0), -2, "up");
        assert_eq!(clicks(&mut residue, 0.0, -3), -3);
    }

    #[test]
    fn an_inputs_x_time_is_the_servers_less_its_age() {
        assert_eq!(x_time(1_000_000, 1_004_400, 500), Some(496));
        assert_eq!(x_time(1_000_000, 1_000_000, 0), Some(0));
        assert_eq!(x_time(1_000_000, 999_000, 500), None, "from the future");
        assert_eq!(x_time(1, 20_000_000, 500), None, "not this clock");
    }

    #[test]
    fn relative_motions_add_up_and_keep_the_latest_time() {
        let mut seat = Seat::default();
        let motion = |high: u32, low: u32, dx: f64| {
            [
                Value::Uint(high),
                Value::Uint(low),
                Value::Fixed(dx * 2.0),
                Value::Fixed(0.0),
                Value::Fixed(dx),
                Value::Fixed(-1.0),
            ]
        };
        let at = zwp_relative_pointer_v1::event::RELATIVE_MOTION;
        seat.relative_motion(at, &motion(0, 10, 3.0));
        seat.relative_motion(at, &motion(1, 20, 4.0));
        assert_eq!(seat.moved, Some(((1 << 32) | 20, 7.0, -2.0)));
        seat.relative_motion(at + 1, &motion(0, 30, 1.0));
        assert_eq!(seat.moved, Some(((1 << 32) | 20, 7.0, -2.0)));
    }

    #[test]
    fn an_x_keycode_is_evdev_plus_eight() {
        assert_eq!(x_keycode(30), Some(38), "KEY_A is X's 38");
        assert_eq!(x_keycode(247), Some(255));
        assert_eq!(x_keycode(248), None);
    }

    #[test]
    fn a_cursor_is_premultiplied() {
        let straight = [
            0xff, 0x80, 0x00, 0x80, 0x10, 0x20, 0x30, 0xff, 0xff, 0xff, 0xff, 0,
        ];
        assert_eq!(
            premultiplied(&straight),
            [0x80, 0x40, 0x00, 0x80, 0x10, 0x20, 0x30, 0xff, 0, 0, 0, 0]
        );
    }
}
