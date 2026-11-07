//! The compositor's seat, into X: keys, the pointer, the wheel, the
//! keyboard focus, the cursor and a game's pointer lock.
//!
//! The compositor tells a client about its own windows only: which one the
//! pointer is on and where in it, which one has the keyboard. The core's
//! rootless seat turns that into what an X server that owns the screen would
//! have seen (`yserver_core::rootless`, spec §4.4):
//!
//! * **The pointer** is a position on a top-level's native window, which is
//!   its whole backing at buffer scale 1, so surface-local logical pixels
//!   are backing pixels: `RootlessInput::PointerEnter`, `PointerMotionLocal`
//!   and `PointerLeave`. **Buttons** pass through as evdev codes. **The
//!   wheel** becomes X's buttons 4 to 7, one press and release a click: the
//!   compositor's clicks where it counts them, else ten units of its scroll
//!   a click, which is what a wheel click is in `wl_pointer.axis`.
//! * **Keys** are evdev codes plus 8, cooked by the server's own XKB state,
//!   which took the compositor's layout at start. The toolkit's own repeat
//!   is dropped: the X server repeats a held key itself.
//! * **Focus.** The keyboard entering a window is `HostRequest::Activated`,
//!   leaving it `Deactivated`.
//! * **The cursor** is the X cursor in effect, given with
//!   `wl_pointer.set_cursor` whenever it changes, premultiplied.
//! * **The input's time**: where the compositor serves
//!   `zwp_relative_pointer_v1`, the host asks for one on a pointer of its
//!   own. Its `relative_motion` carries the time the device stamped the input
//!   with, in microseconds of `CLOCK_MONOTONIC`; the pointer motion it comes
//!   before is handed to X with that time, as server milliseconds. With
//!   `YSERVER_INPUT_AGES=1` the host logs `input-age stamp <us> xtime <ms>
//!   age <us>` as it does.
//! * **A game's pointer lock** (`RootlessEvent::PointerLock`, the core's
//!   decision): the compositor's `zwp_locked_pointer_v1` on the game's
//!   window, asked for as `persistent`, as Xwayland asks; while it is in
//!   force each relative motion is `RootlessInput::PointerRelative`. With
//!   `YSERVER_POINTER_LOCK=0` the host never locks.

use std::collections::{BTreeSet, HashMap};

use compositor_toolkit::{
    Client, CursorShape, Event, KeyboardEvent, ObjectId, PointerEvent, SurfaceId, Value,
    protocol::{
        core::{WL_POINTER, wl_seat},
        pointer_constraints::{
            ZWP_LOCKED_POINTER_V1, ZWP_POINTER_CONSTRAINTS_V1, zwp_locked_pointer_v1,
            zwp_pointer_constraints_v1,
        },
        relative_pointer::{
            ZWP_RELATIVE_POINTER_MANAGER_V1, ZWP_RELATIVE_POINTER_V1,
            zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
        },
    },
};
use yserver_core::{
    backend::ActiveCursorImage,
    core_loop::{
        DeviceInfo, HostInputEvent, InputOrigin,
        message::{
            LibinputConfigSnapshot, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
            SYNTH_SCROLL_UP,
        },
    },
    host_x11::HostKeyEvent,
    rootless::{HostRequest, RootlessInput, ToplevelId},
    xinput::{InputCapabilities, InputSourceId},
};

use crate::rootless::{display::HostWork, present_clock::monotonic_us};

/// The compositor's seat as X sees it: a slave pointer and a slave
/// keyboard of the core pair, as Xwayland's `xwayland-pointer` and
/// `xwayland-keyboard`, announced before any input ([`announcements`]).
/// Their input carries their own ids as its source, as any slave's does:
/// an event sourced from a master is one no X server sends, and a client
/// that opens an event's source (Wine opens a raw button's with
/// XOpenDevice) is refused one.
///
/// The ids are the top of the source space, clear of libinput's, which
/// count up from 1.
const POINTER: InputSourceId = InputSourceId(u64::MAX - 1);
const KEYBOARD: InputSourceId = InputSourceId(u64::MAX);

/// The origin of the compositor's pointer input.
const ORIGIN: InputOrigin = InputOrigin::Physical(POINTER);

/// The origin of the compositor's keys.
const KEY_ORIGIN: InputOrigin = InputOrigin::Physical(KEYBOARD);

/// The seat's two devices, for the core before any input: XI slaves of the
/// core pointer and keyboard, as the window-system shells' are
/// (`surface/input/devices.rs`).
pub(super) fn announcements() -> [HostInputEvent; 2] {
    let device = |source_id, name: &str, keyboard| {
        HostInputEvent::DeviceAdded(DeviceInfo {
            source_id,
            enabled: true,
            resume_key: None,
            capabilities: InputCapabilities {
                keyboard,
                pointer: !keyboard,
                touch: false,
            },
            name: name.to_owned(),
            device_node: String::new(),
            sysname: String::new(),
            vendor_id: 0,
            product_id: 0,
            is_touchpad: false,
            config: LibinputConfigSnapshot::default(),
        })
    };
    [
        device(POINTER, "Wayland pointer", false),
        device(KEYBOARD, "Wayland keyboard", true),
    ]
}

/// How much `wl_pointer.axis` scroll is one wheel click, where the
/// compositor does not count clicks itself.
const SCROLL_PER_CLICK: f64 = 10.0;

/// The oldest a relative motion's time may be and still be taken as the
/// input's: a compositor whose `relative_motion` time is not
/// `CLOCK_MONOTONIC` gives a time far from now, and then the motion is handed
/// to X at the server's time instead.
const OLDEST_STAMP_US: u64 = 10_000_000;

/// A cursor as given: its X cursor, its version and its hotspot.
type CursorKey = (u32, u32, (u16, u16));

/// A `zwp_locked_pointer_v1` the host asked for.
#[derive(Debug)]
struct Lock {
    object: ObjectId,
    id: ToplevelId,
    /// The compositor said `locked` and not `unlocked` since.
    active: bool,
}

/// The seat's state between dispatches.
#[derive(Debug, Default)]
pub(super) struct Seat {
    /// What the compositor said since the last [`Seat::deliver`].
    queue: Vec<Event>,
    /// The compositor's window the pointer is on.
    pointer: Option<SurfaceId>,
    /// Where on it, in its surface's pixels.
    at: (f64, f64),
    /// Evdev buttons the server has down.
    buttons: BTreeSet<u32>,
    /// X keycodes the server has down.
    keys: BTreeSet<u8>,
    /// Scroll that is not yet a whole click: down, right.
    scroll: (f64, f64),
    /// The cursor last given: its version and hotspot, or none; `None`
    /// before the first.
    cursor: Option<Option<CursorKey>>,
    relative_manager: Option<ObjectId>,
    /// The `zwp_relative_pointer_v1` on a pointer of the host's own.
    relative: Option<ObjectId>,
    /// The seat has had a pointer, which is when one may be asked for.
    pointed: bool,
    /// The relative motion since a pointer motion was last handed to X: the
    /// latest one's time in microseconds of `CLOCK_MONOTONIC`.
    moved: Option<u64>,
    ages: bool,
    /// The `wl_pointer` of the host's own the relative pointer was made on,
    /// which a lock names.
    own_pointer: Option<ObjectId>,
    /// The compositor's `zwp_pointer_constraints_v1`, where it serves one
    /// and locking is not turned off.
    constraints: Option<ObjectId>,
    lock: Option<Lock>,
}

impl Seat {
    /// Bind the compositor's relative pointer manager and pointer
    /// constraints, where it has them.
    pub(super) fn bind(&mut self, client: &mut Client) {
        self.relative_manager = client
            .bind(&ZWP_RELATIVE_POINTER_MANAGER_V1, 1, None)
            .ok()
            .map(|(manager, _)| manager);
        if self.relative_manager.is_none() {
            log::info!("wayland: no zwp_relative_pointer_manager_v1; input is stamped on arrival");
        }
        self.ages = std::env::var("YSERVER_INPUT_AGES").is_ok_and(|value| value == "1");
        if std::env::var("YSERVER_POINTER_LOCK").is_ok_and(|value| value == "0") {
            log::info!("wayland: YSERVER_POINTER_LOCK=0; the pointer is never locked");
            return;
        }
        self.constraints = client
            .bind(&ZWP_POINTER_CONSTRAINTS_V1, 1, None)
            .ok()
            .map(|(constraints, _)| constraints);
        if self.constraints.is_none() {
            log::info!("wayland: no zwp_pointer_constraints_v1; the pointer is never locked");
        }
    }

    /// Keep a seat event for [`Seat::deliver`]; anything else back.
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
            Event::Object { object, .. }
                if Some(object) == self.relative
                    || self.lock.as_ref().is_some_and(|lock| lock.object == object) =>
            {
                self.queue.push(event);
                None
            }
            other => Some(other),
        }
    }

    /// The compositor's window the pointer is on.
    pub(super) fn pointer_surface(&self) -> Option<SurfaceId> {
        self.pointer
    }

    fn locked(&self) -> bool {
        self.lock.as_ref().is_some_and(|lock| lock.active)
    }

    /// Hand what the seat did since the last call to the core.
    pub(super) fn deliver(
        &mut self,
        client: &mut Client,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
    ) {
        for event in std::mem::take(&mut self.queue) {
            match event {
                Event::Pointer(event) => self.pointer_event(work, windows, event),
                Event::Keyboard(event) => self.keyboard_event(work, windows, event),
                Event::Object { object, opcode, .. }
                    if self.lock.as_ref().is_some_and(|lock| lock.object == object) =>
                {
                    self.lock_event(opcode);
                }
                Event::Object { opcode, args, .. } => self.relative_motion(work, opcode, &args),
                _ => {}
            }
        }
        self.ask_relative(client);
    }

    /// Ask for the relative pointer once the seat has a pointer: on a
    /// `wl_pointer` of the host's own, since the client runtime keeps its
    /// own to itself. The compositor sends relative motion to every relative
    /// pointer of the client the pointer is over; the second pointer's own
    /// events are not read.
    fn ask_relative(&mut self, client: &mut Client) {
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
            Ok(()) => {
                self.relative = Some(id);
                self.own_pointer = Some(pointer);
            }
            Err(error) => {
                log::warn!("wayland: the relative pointer: {error}");
                self.relative_manager = None;
            }
        }
    }

    /// The X time of the pending motion, from its relative motion's stamp,
    /// with that stamp; the server's now without one.
    fn motion_time(&mut self) -> (u32, Option<u64>) {
        let server = crate::clock::server_time_ms();
        let stamped = self
            .moved
            .take()
            .and_then(|stamp| Some((x_time(stamp, monotonic_us(), server)?, stamp)));
        if self.ages
            && let Some((time, stamp)) = stamped
        {
            log::info!(
                "input-age stamp {stamp} xtime {time} age {}",
                monotonic_us().saturating_sub(stamp)
            );
        }
        stamped.map_or((server, None), |(time, stamp)| (time, Some(stamp)))
    }

    fn motion(
        &mut self,
        work: &mut HostWork,
        toplevel: ToplevelId,
        local: (f64, f64),
        enter: bool,
    ) {
        let (time, _) = self.motion_time();
        let input = if enter {
            RootlessInput::PointerEnter {
                origin: ORIGIN,
                toplevel,
                local,
                time,
            }
        } else {
            RootlessInput::PointerMotionLocal {
                origin: ORIGIN,
                toplevel,
                local,
                time,
            }
        };
        work.input.push(HostInputEvent::Rootless(input));
    }

    fn pointer_event(
        &mut self,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
        event: PointerEvent,
    ) {
        match event {
            PointerEvent::Enter { surface, x, y } => {
                self.pointer = Some(surface);
                self.at = (x, y);
                match windows.get(&surface) {
                    Some(&(toplevel, crop)) => self.motion(work, toplevel, local(x, y, crop), true),
                    None => self.leave(work),
                }
            }
            PointerEvent::Leave { .. } => {
                self.pointer = None;
                self.leave(work);
            }
            // A held pointer does not move; X's moves by the relative
            // motion.
            PointerEvent::Motion { .. } if self.locked() => {}
            PointerEvent::Motion { surface, x, y } => {
                self.at = (x, y);
                if let Some(&(toplevel, crop)) = windows.get(&surface) {
                    self.motion(work, toplevel, local(x, y, crop), false);
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
                if !self.locked()
                    && (x, y) != self.at
                    && let Some(&(toplevel, crop)) = windows.get(&surface)
                {
                    self.at = (x, y);
                    self.motion(work, toplevel, local(x, y, crop), false);
                }
                let changed = if pressed {
                    self.buttons.insert(button)
                } else {
                    self.buttons.remove(&button)
                };
                if changed {
                    press(work, button, pressed);
                }
            }
            PointerEvent::Axis {
                vertical,
                horizontal,
                discrete,
                ..
            } => {
                let down = clicks(&mut self.scroll.0, vertical, discrete.0);
                let right = clicks(&mut self.scroll.1, horizontal, discrete.1);
                for (count, forward, backward) in [
                    (down, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_UP),
                    (right, SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_LEFT),
                ] {
                    let button = if count > 0 { forward } else { backward };
                    for _ in 0..count.unsigned_abs() {
                        press(work, u32::from(button), true);
                        press(work, u32::from(button), false);
                    }
                }
            }
        }
    }

    /// Off every X window: the core releases the buttons X has down and
    /// moves the pointer to the root.
    fn leave(&mut self, work: &mut HostWork) {
        self.buttons.clear();
        let (time, _) = self.motion_time();
        work.input
            .push(HostInputEvent::Rootless(RootlessInput::PointerLeave {
                origin: ORIGIN,
                time,
            }));
    }

    /// One `relative_motion`: its time is the next pointer motion's; while
    /// the lock holds, its distance moves X's pointer.
    fn relative_motion(&mut self, work: &mut HostWork, opcode: u16, args: &[Value]) {
        if opcode != zwp_relative_pointer_v1::event::RELATIVE_MOTION {
            return;
        }
        let uint = |at: usize| args.get(at).and_then(Value::as_uint).unwrap_or(0);
        let fixed = |at: usize| match args.get(at) {
            Some(Value::Fixed(value)) => *value,
            _ => 0.0,
        };
        let stamp = (u64::from(uint(0)) << 32) | u64::from(uint(1));
        self.moved = Some(stamp);
        if !self.locked() {
            return;
        }
        let (time, _) = self.motion_time();
        work.input
            .push(HostInputEvent::Rootless(RootlessInput::PointerRelative {
                origin: ORIGIN,
                delta: (fixed(2), fixed(3)),
                unaccelerated: (fixed(4), fixed(5)),
                time,
            }));
    }

    fn keyboard_event(
        &mut self,
        work: &mut HostWork,
        windows: &HashMap<SurfaceId, (ToplevelId, u16)>,
        event: KeyboardEvent,
    ) {
        match event {
            KeyboardEvent::Enter(surface) => {
                if let Some(&(id, _)) = windows.get(&surface) {
                    log::info!("wayland: the keyboard is on window {id:?}");
                    work.requests.push(HostRequest::Activated { id });
                }
            }
            KeyboardEvent::Leave(_) => {
                // The core lets go of every key X still has down.
                self.keys.clear();
                work.requests.push(HostRequest::Deactivated);
            }
            KeyboardEvent::Key(key) => {
                if key.repeat {
                    return;
                }
                let Some(keycode) = x_keycode(key.code) else {
                    log::debug!("wayland: key {} has no X keycode", key.code);
                    return;
                };
                let changed = if key.pressed {
                    self.keys.insert(keycode)
                } else {
                    self.keys.remove(&keycode)
                };
                if changed {
                    work.input.push(HostInputEvent::Key(HostKeyEvent {
                        origin: KEY_ORIGIN,
                        pressed: key.pressed,
                        keycode,
                        time: crate::clock::server_time_ms(),
                        root_x: 0,
                        root_y: 0,
                        event_x: 0,
                        event_y: 0,
                        state: 0,
                    }));
                }
            }
            // The server's XKB state follows the keys themselves.
            KeyboardEvent::Modifiers(_) => {}
        }
    }

    /// The compositor's word on the lock: `locked` or `unlocked`.
    fn lock_event(&mut self, opcode: u16) {
        let Some(lock) = self.lock.as_mut() else {
            return;
        };
        let active = opcode == zwp_locked_pointer_v1::event::LOCKED;
        if active != lock.active {
            log::info!(
                "wayland: the compositor {} the pointer on window {:?}",
                if active { "locked" } else { "unlocked" },
                lock.id
            );
        }
        lock.active = active;
    }

    /// Lock the pointer on `target`'s window (the core's decision), or let
    /// it go.
    pub(super) fn lock(&mut self, client: &mut Client, target: Option<(ToplevelId, SurfaceId)>) {
        if let Some(lock) = self.lock.take() {
            log::info!(
                "wayland: the pointer lock on window {:?} is let go",
                lock.id
            );
            if let Err(error) =
                client.request(lock.object, zwp_locked_pointer_v1::request::DESTROY, &[])
            {
                log::warn!("wayland: letting the pointer go: {error}");
            }
        }
        let (Some((id, surface)), Some(constraints), Some(pointer)) =
            (target, self.constraints, self.own_pointer)
        else {
            return;
        };
        let Some(surface) = client.surface_object(surface) else {
            return;
        };
        let object = client.new_object(&ZWP_LOCKED_POINTER_V1, 1);
        let asked = client.request(
            constraints,
            zwp_pointer_constraints_v1::request::LOCK_POINTER,
            &[
                Value::NewId(object),
                Value::Object(surface),
                Value::Object(pointer),
                Value::Object(ObjectId::NULL),
                Value::Uint(zwp_pointer_constraints_v1::lifetime::PERSISTENT),
            ],
        );
        match asked {
            Ok(()) => {
                log::info!("wayland: asked to lock the pointer on window {id:?}");
                self.lock = Some(Lock {
                    object,
                    id,
                    active: false,
                });
            }
            Err(error) => {
                log::warn!("wayland: locking the pointer: {error}");
                self.constraints = None;
            }
        }
    }

    /// Window `id` is gone: a lock on it goes with it.
    pub(super) fn forget(&mut self, client: &mut Client, id: ToplevelId) {
        if self.lock.as_ref().is_some_and(|lock| lock.id == id) {
            self.lock(client, None);
        }
    }

    /// Give the compositor the X cursor in effect, or hide it.
    pub(super) fn set_cursor(&mut self, client: &mut Client, cursor: Option<&ActiveCursorImage>) {
        let key = cursor.map(|image| (image.host_xid, image.serial, (image.hot_x, image.hot_y)));
        if self.cursor == Some(key) {
            return;
        }
        self.cursor = Some(key);
        let Some(image) = cursor else {
            client.hide_cursor();
            return;
        };
        let pixels = premultiplied(&image.bgra_bytes);
        match client.set_cursor_image(
            u32::from(image.width),
            u32::from(image.height),
            (i32::from(image.hot_x), i32::from(image.hot_y)),
            &pixels,
        ) {
            Ok(()) => log::debug!(
                "wayland: the cursor is {}x{} at ({}, {}), X cursor 0x{:x}",
                image.width,
                image.height,
                image.hot_x,
                image.hot_y,
                image.host_xid
            ),
            Err(error) => {
                log::warn!("wayland: the cursor: {error}");
                client.set_cursor(CursorShape::default());
            }
        }
    }
}

/// A position on a native window, in its backing's pixels: the window
/// leaves `crop` of the backing's edge out.
fn local(x: f64, y: f64, crop: u16) -> (f64, f64) {
    (x + f64::from(crop), y + f64::from(crop))
}

/// Press or let go of pointer button `button` (an evdev code, or one of the
/// server's synthetic scroll codes).
fn press(work: &mut HostWork, button: u32, pressed: bool) {
    let Ok(button) = u16::try_from(button) else {
        return;
    };
    work.input.push(HostInputEvent::PointerButton {
        origin: ORIGIN,
        button,
        pressed,
        time: crate::clock::server_time_ms(),
    });
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

/// Whole wheel clicks from one axis event: the compositor's own count when
/// it gives one, which also forgets what was left over, else `value` added
/// to `residue`, a click every [`SCROLL_PER_CLICK`].
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

/// A cursor's straight-alpha B, G, R, A pixels as `wl_shm`'s premultiplied
/// ARGB8888, which is B, G, R, A in memory too.
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

    /// The pointer's and the keys' input comes from the two slaves the host
    /// announces, never from a master (`InputOrigin::NestedHost`), whose
    /// events carry the master as their source.
    #[test]
    fn the_seats_input_comes_from_the_slaves_it_announces() {
        let [
            HostInputEvent::DeviceAdded(pointer),
            HostInputEvent::DeviceAdded(keyboard),
        ] = announcements()
        else {
            panic!("two devices");
        };
        assert!(pointer.capabilities.pointer && !pointer.capabilities.keyboard);
        assert!(keyboard.capabilities.keyboard && !keyboard.capabilities.pointer);
        assert_ne!(pointer.source_id, keyboard.source_id);
        assert_eq!(ORIGIN, InputOrigin::Physical(pointer.source_id));
        assert_eq!(KEY_ORIGIN, InputOrigin::Physical(keyboard.source_id));
        assert_eq!(
            (pointer.name.as_str(), keyboard.name.as_str()),
            ("Wayland pointer", "Wayland keyboard")
        );
    }

    #[test]
    fn the_wheel_counts_the_compositors_clicks_or_ten_units_a_click() {
        let mut residue = 0.0;
        assert_eq!(clicks(&mut residue, 15.0, 0), 1);
        assert_eq!(clicks(&mut residue, 5.0, 0), 1);
        assert_eq!(clicks(&mut residue, 3.0, 2), 2);
        assert!(residue.abs() < f64::EPSILON);
        assert_eq!(clicks(&mut residue, -25.0, 0), -2);
    }

    #[test]
    fn an_inputs_x_time_is_the_servers_less_its_age() {
        assert_eq!(x_time(1_000_000, 1_004_000, 500), Some(496));
        assert_eq!(x_time(1_004_000, 1_000_000, 500), None, "from the future");
        assert_eq!(x_time(0, OLDEST_STAMP_US + 1, 500), None, "too old");
    }

    #[test]
    fn an_x_keycode_is_evdev_plus_eight() {
        assert_eq!(x_keycode(30), Some(38));
        assert_eq!(x_keycode(248), None);
    }

    #[test]
    fn a_cursor_is_premultiplied() {
        assert_eq!(premultiplied(&[255, 128, 0, 128]), [128, 64, 0, 128]);
        assert_eq!(premultiplied(&[9, 9, 9, 0]), [0, 0, 0, 0]);
    }
}
