//! What the Wayland backend asks of a Wayland client library: the small
//! interface [`Transport`], which [`super::toolkit`] implements on Ferrix's
//! client runtime (`compositor-toolkit`, over `compositor-wire`,
//! `compositor-protocol` and `compositor-shm`).
//!
//! Everything above it -- the backend, the clipboard bridge, the seat --
//! knows the compositor only through this module's types, so another
//! client library (`wayland-client`) can stand in for the toolkit by
//! implementing [`Transport`], without a change to anything else.
//!
//! A transport is a [`HostWindows`] -- top-levels are `xdg_toplevel`s,
//! images `wl_shm` buffers -- plus the connection, the compositor's events,
//! its screen and keyboard layout, and the drag of a window.

use std::{io, os::fd::RawFd};

use yserver_core::rootless::HostWindows;

/// A window of the compositor's: a top-level surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Surface(pub u32);

/// The compositor's first screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    /// Its size in logical pixels.
    pub size: (u16, u16),
    /// How often it refreshes, in millihertz; 0 when it said nothing.
    pub refresh_mhz: u32,
}

/// What the compositor said.
#[derive(Debug)]
pub enum Event {
    /// It is ready for another frame of `surface`.
    Frame(Surface),
    /// It gave a top-level a size.
    Configure {
        /// Which.
        surface: Surface,
        /// Its width.
        width: u32,
        /// Its height.
        height: u32,
    },
    /// It asked to close a top-level (`xdg_toplevel.close`).
    Close(Surface),
}

/// A connection to a Wayland compositor, as the backend uses it.
pub trait Transport: HostWindows<Surface = Surface> {
    /// The socket, for the core loop's poller.
    fn fd(&self) -> RawFd;

    /// Read what the compositor sent, without blocking.
    ///
    /// # Errors
    ///
    /// The compositor closed the connection or refused a request.
    fn dispatch(&mut self) -> io::Result<Vec<Event>>;

    /// Wait until the compositor has answered everything sent so far.
    ///
    /// # Errors
    ///
    /// As [`Transport::dispatch`].
    fn roundtrip(&mut self) -> io::Result<Vec<Event>>;

    /// Send what is queued.
    ///
    /// # Errors
    ///
    /// The connection failed.
    fn flush(&mut self) -> io::Result<()>;

    /// The compositor's first screen, once it has described one.
    fn screen(&self) -> Option<Screen>;

    /// The layout and variant of the keymap the compositor sent, by name.
    fn keyboard_layout(&self) -> Option<(String, String)>;

    /// Hand a top-level to the compositor's interactive move or resize
    /// (`xdg_toplevel.move`, `resize` with `edges`, 0 a move), from the
    /// last press the compositor sent.
    fn drag(&mut self, surface: Surface, edges: u32);
}
