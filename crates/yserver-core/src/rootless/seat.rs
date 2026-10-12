//! The host's seat, into X: the pointer, its buttons and wheel, the keys and
//! the keyboard focus.
//!
//! A host tells its client about its own windows only: which one the
//! pointer is on and where in it, which one has the keyboard. [`Seat`]
//! turns that into what an X server that owns the screen would have seen,
//! through `handle_host_input`, as a KMS server's libinput thread's events
//! go, so grabs, key repeat, XKB and XI2 apply unchanged.
//!
//! * **The pointer.** An X top-level keeps the root position X gave it,
//!   wherever the host shows it, so a position on a host window is that
//!   window's X origin plus the position on it. The hit test is kept to the
//!   top-level the pointer is on ([`PointerScope`]), because two X windows
//!   the host shows apart can overlap in root coordinates; off every X
//!   window the pointer is on the root, which gives the last window its
//!   `LeaveNotify`.
//! * **Buttons** are evdev codes, as libinput gives them. **The wheel**
//!   becomes X's buttons 4 to 7, one press and release a click: the host's
//!   clicks where it counts them, else ten units of its scroll a click.
//! * **Keys** are X keycodes, cooked by the server's own XKB state; a host
//!   that does not repeat a held key itself leaves that to the server.
//! * **Focus.** The keyboard entering a host window gives its X top-level
//!   the input focus, as a window manager would; leaving takes it away
//!   (focus None) and lets go of every key the server still has down.

use std::collections::BTreeSet;

use log::{debug, info};

use super::Topology;
use crate::{
    backend::Backend,
    core_loop::{
        handle_host_input,
        message::{
            HostInputEvent, InputOrigin, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
            SYNTH_SCROLL_UP,
        },
        process_request::set_input_focus_for_server,
    },
    host_x11::HostKeyEvent,
    server::{PointerScope, ServerState},
};

/// How much scroll is one wheel click, where the host does not count
/// clicks itself (`wl_pointer.axis`'s units).
const SCROLL_PER_CLICK: f64 = 10.0;

/// The seat's state between events.
#[derive(Debug, Default)]
pub struct Seat {
    /// Where the pointer last was, in root coordinates.
    at: (i32, i32),
    /// Evdev buttons the server has down.
    buttons: BTreeSet<u32>,
    /// X keycodes the server has down.
    keys: BTreeSet<u8>,
    /// Scroll that is not yet a whole click: down, right.
    scroll: (f64, f64),
}

impl Seat {
    /// Where the pointer is, in root coordinates.
    #[must_use]
    pub fn position(&self) -> (i32, i32) {
        self.at
    }

    /// The pointer came onto the host window `surface`, at (`x`, `y`) on it.
    pub fn pointer_enter<S: Copy + Eq + std::fmt::Debug>(
        &mut self,
        topology: &mut Topology<S>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        (surface, x, y): (S, f64, f64),
        time: u32,
    ) {
        topology.set_pointer_surface(Some(surface));
        state.pointer_scope = topology
            .window_of(state, surface)
            .map_or(PointerScope::Nowhere, PointerScope::Within);
        if let Some(at) = root_position(topology, state, surface, x, y) {
            self.at = at;
        }
        self.move_pointer(state, backend, time);
    }

    /// The pointer left the server's windows: every button it held is let
    /// go, and X has it on the root.
    pub fn pointer_leave<S: Copy + Eq + std::fmt::Debug>(
        &mut self,
        topology: &mut Topology<S>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        time: u32,
    ) {
        topology.set_pointer_surface(None);
        for button in std::mem::take(&mut self.buttons) {
            press(state, backend, button, false, time);
        }
        state.pointer_scope = PointerScope::Nowhere;
        self.move_pointer(state, backend, time);
    }

    /// The pointer moved to (`x`, `y`) on `surface`.
    pub fn pointer_motion<S: Copy + Eq + std::fmt::Debug>(
        &mut self,
        topology: &Topology<S>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        (surface, x, y): (S, f64, f64),
        time: u32,
    ) {
        if let Some(at) = root_position(topology, state, surface, x, y) {
            self.at = at;
            self.move_pointer(state, backend, time);
        }
    }

    /// The evdev button `button` went down or up at (`x`, `y`) on
    /// `surface`, where the pointer is moved first if it is not there.
    pub fn pointer_button<S: Copy + Eq + std::fmt::Debug>(
        &mut self,
        topology: &Topology<S>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        (surface, x, y): (S, f64, f64),
        (button, pressed): (u32, bool),
        time: u32,
    ) {
        if let Some(at) = root_position(topology, state, surface, x, y)
            && at != self.at
        {
            self.at = at;
            self.move_pointer(state, backend, time);
        }
        let changed = if pressed {
            self.buttons.insert(button)
        } else {
            self.buttons.remove(&button)
        };
        if changed {
            press(state, backend, button, pressed, time);
        }
    }

    /// The wheel turned: `vertical` and `horizontal` in the host's scroll
    /// units (down and right positive), `discrete` the clicks the host
    /// counted, 0 where it counted none.
    pub fn axis(
        &mut self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        (vertical, horizontal): (f64, f64),
        discrete: (i32, i32),
        time: u32,
    ) {
        let down = clicks(&mut self.scroll.0, vertical, discrete.0);
        let right = clicks(&mut self.scroll.1, horizontal, discrete.1);
        for (count, forward, backward) in [
            (down, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_UP),
            (right, SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_LEFT),
        ] {
            let button = if count > 0 { forward } else { backward };
            for _ in 0..count.unsigned_abs() {
                press(state, backend, u32::from(button), true, time);
                press(state, backend, u32::from(button), false, time);
            }
        }
    }

    /// The keyboard came onto the host window `surface`: its X top-level
    /// takes the focus.
    pub fn keyboard_enter<S: Copy + Eq + std::fmt::Debug>(
        &mut self,
        topology: &Topology<S>,
        state: &mut ServerState,
        surface: S,
    ) {
        let Some(window) = topology.window_of(state, surface) else {
            return;
        };
        if set_input_focus_for_server(state, Some(window)) {
            info!("rootless: the keyboard is on window 0x{:x}", window.0);
        }
    }

    /// The keyboard left the server's windows: every key still down is let
    /// go, and the focus is None.
    pub fn keyboard_leave(
        &mut self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        time: u32,
    ) {
        for keycode in std::mem::take(&mut self.keys) {
            self.key_event(state, backend, keycode, false, time);
        }
        let _ = set_input_focus_for_server(state, None);
    }

    /// The X keycode `keycode` went down or up.
    pub fn key(
        &mut self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        keycode: u8,
        pressed: bool,
        time: u32,
    ) {
        let changed = if pressed {
            self.keys.insert(keycode)
        } else {
            self.keys.remove(&keycode)
        };
        if changed {
            self.key_event(state, backend, keycode, pressed, time);
        } else {
            debug!("rootless: key {keycode} is {pressed} already");
        }
    }

    fn key_event(
        &self,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        keycode: u8,
        pressed: bool,
        time: u32,
    ) {
        let (x, y) = self.at;
        let x = i16::try_from(x).unwrap_or(i16::MAX);
        let y = i16::try_from(y).unwrap_or(i16::MAX);
        handle_host_input(
            state,
            backend,
            HostInputEvent::Key(HostKeyEvent {
                origin: InputOrigin::NestedHost,
                pressed,
                keycode,
                time,
                root_x: x,
                root_y: y,
                event_x: x,
                event_y: y,
                state: 0,
            }),
        );
    }

    /// Tell X the pointer is at [`Seat::at`], which also recomputes the
    /// window it is in and sends the crossings.
    fn move_pointer(&self, state: &mut ServerState, backend: &mut dyn Backend, time: u32) {
        let (x, y) = self.at;
        handle_host_input(
            state,
            backend,
            HostInputEvent::PointerMotion {
                origin: InputOrigin::NestedHost,
                x,
                y,
                time,
                relative: false,
                dx: 0,
                dy: 0,
                motion_delta: None,
            },
        );
    }
}

/// Where a position on the host window `surface` is on the root.
fn root_position<S: Copy + Eq + std::fmt::Debug>(
    topology: &Topology<S>,
    state: &ServerState,
    surface: S,
    x: f64,
    y: f64,
) -> Option<(i32, i32)> {
    let window = topology.window_of(state, surface)?;
    let origin = state.resources.window_absolute_position(window);
    Some(root_point(origin, (x, y)))
}

/// Press or let go of pointer button `button` (an evdev code, or one of
/// the server's synthetic scroll codes).
fn press(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    button: u32,
    pressed: bool,
    time: u32,
) {
    let Ok(button) = u16::try_from(button) else {
        return;
    };
    handle_host_input(
        state,
        backend,
        HostInputEvent::PointerButton {
            origin: InputOrigin::NestedHost,
            button,
            pressed,
            time,
        },
    );
}

/// A position on a window whose X origin is `origin`, on the root.
fn root_point(origin: (i32, i32), (x, y): (f64, f64)) -> (i32, i32) {
    #[allow(clippy::cast_possible_truncation)]
    let local = (x.floor() as i32, y.floor() as i32);
    (
        origin.0.saturating_add(local.0),
        origin.1.saturating_add(local.1),
    )
}

/// Whole wheel clicks from one axis event: the host's own count when it
/// gives one, which also forgets what was left over, else `value` added to
/// `residue`, a click every [`SCROLL_PER_CLICK`].
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_on_a_window_is_its_origin_plus_the_local_one() {
        assert_eq!(root_point((100, 50), (10.7, 0.2)), (110, 50));
        assert_eq!(root_point((0, 0), (-0.5, 3.0)), (-1, 3));
    }

    #[test]
    fn the_wheel_counts_the_hosts_clicks_or_ten_units_a_click() {
        let mut residue = 0.0;
        assert_eq!(clicks(&mut residue, 15.0, 1), 1, "its own count");
        assert_eq!(residue, 0.0);
        assert_eq!(clicks(&mut residue, 4.0, 0), 0);
        assert_eq!(clicks(&mut residue, 7.0, 0), 1, "eleven units: a click");
        assert!((residue - 1.0).abs() < 1e-9);
        assert_eq!(clicks(&mut residue, -21.0, 0), -2, "up");
        assert_eq!(clicks(&mut residue, 0.0, -3), -3);
    }
}
