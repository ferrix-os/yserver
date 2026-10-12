//! The compositor's seat, into X: its pointer and keyboard events handed to
//! `yserver_core::rootless::seat`, which does what is the same for every
//! host, and the X cursor in effect given to the compositor.
//!
//! What is Wayland's here: a key is `wl_keyboard.key`'s evdev code, which
//! is 8 less than X's keycode; a repeat the client library makes is
//! dropped, since the X server repeats a held key itself, per
//! `ChangeKeyboardControl`; and the cursor is copied into a buffer of the
//! compositor's and given with `wl_pointer.set_cursor` whenever it changes,
//! or the compositor's own arrow with none.

use yserver_core::{backend::Backend, rootless::Topology, server::ServerState};

use super::transport::{CursorImage, KeyboardEvent, PointerEvent, Surface, Transport};
use crate::rootless::Renderer;

/// The seat's state between loop iterations.
#[derive(Debug, Default)]
pub(super) struct Seat {
    seat: yserver_core::rootless::seat::Seat,
    /// The cursor last given to the compositor
    /// (`RootlessRenderer::cursor_key`); the outer `None` before the first.
    cursor: Option<Option<(u32, u64)>>,
}

impl Seat {
    /// Hand a pointer event to X.
    pub(super) fn pointer(
        &mut self,
        topology: &mut Topology<Surface>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        event: PointerEvent,
    ) {
        let time = crate::clock::server_time_ms();
        let seat = &mut self.seat;
        match event {
            PointerEvent::Enter { surface, x, y } => {
                seat.pointer_enter(topology, state, backend, (surface, x, y), time);
            }
            PointerEvent::Leave => seat.pointer_leave(topology, state, backend, time),
            PointerEvent::Motion { surface, x, y } => {
                seat.pointer_motion(topology, state, backend, (surface, x, y), time);
            }
            PointerEvent::Button {
                surface,
                x,
                y,
                button,
                pressed,
            } => seat.pointer_button(
                topology,
                state,
                backend,
                (surface, x, y),
                (button, pressed),
                time,
            ),
            PointerEvent::Axis {
                vertical,
                horizontal,
                discrete,
            } => seat.axis(state, backend, (vertical, horizontal), discrete, time),
        }
    }

    /// Hand a keyboard event to X.
    pub(super) fn keyboard(
        &mut self,
        topology: &mut Topology<Surface>,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        event: KeyboardEvent,
    ) {
        let time = crate::clock::server_time_ms();
        match event {
            KeyboardEvent::Enter(surface) => self.seat.keyboard_enter(topology, state, surface),
            KeyboardEvent::Leave => self.seat.keyboard_leave(state, backend, time),
            KeyboardEvent::Key {
                code,
                pressed,
                repeat,
            } => {
                // The server repeats a held key itself.
                if repeat {
                    return;
                }
                let Some(keycode) = x_keycode(code) else {
                    log::debug!("wayland: key {code} has no X keycode");
                    return;
                };
                self.seat.key(state, backend, keycode, pressed, time);
            }
        }
    }

    /// Give the compositor the X cursor in effect if it changed since it was
    /// last given. Once per loop iteration.
    pub(super) fn sync_cursor<T: Transport>(
        &mut self,
        transport: &mut T,
        renderer: &mut dyn Renderer,
    ) {
        let key = renderer.cursor_key();
        if self.cursor == Some(key) {
            return;
        }
        self.cursor = Some(key);
        let Some(image) = key.and_then(|_| renderer.get_active_cursor_image()) else {
            let _ = transport.set_cursor(None);
            return;
        };
        let pixels = premultiplied(&image.bgra_bytes);
        let given = transport.set_cursor(Some(CursorImage {
            width: u32::from(image.width),
            height: u32::from(image.height),
            hot: (i32::from(image.hot_x), i32::from(image.hot_y)),
            pixels: &pixels,
        }));
        match given {
            Ok(()) => log::debug!(
                "wayland: the cursor is {}x{} at ({}, {})",
                image.width,
                image.height,
                image.hot_x,
                image.hot_y
            ),
            Err(error) => {
                log::warn!("wayland: the cursor: {error}");
                let _ = transport.set_cursor(None);
            }
        }
    }
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
