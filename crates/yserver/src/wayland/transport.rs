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
//! its screen and keyboard layout, the drag of a window, and the clipboard
//! of `ext_data_control_v1`.

use std::{
    io,
    os::fd::{BorrowedFd, OwnedFd, RawFd},
};

use yserver_core::rootless::HostWindows;

/// A window of the compositor's: a top-level surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Surface(pub u32);

/// The compositor's offer of a selection (`ext_data_control_offer_v1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Offer(pub u32);

/// A selection the server offers the compositor
/// (`ext_data_control_source_v1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Source(pub u32);

/// Which of the compositor's selections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selection {
    /// The clipboard (`set_selection`).
    Clipboard,
    /// The primary selection (`set_primary_selection`).
    Primary,
}

impl Selection {
    /// Both, the clipboard first.
    pub const BOTH: [Self; 2] = [Self::Clipboard, Self::Primary];

    /// 0 for the clipboard, 1 for the primary selection.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Clipboard => 0,
            Self::Primary => 1,
        }
    }
}

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
    /// A selection now holds `offer`, which can be had as `types`; `None`
    /// for an empty one.
    Selection {
        /// Which selection.
        which: Selection,
        /// What it holds.
        offer: Option<Offer>,
        /// The types the offer said it has.
        types: Vec<String>,
    },
    /// A paste of the server's `source` as `mime`, to be written to `fd`.
    Send {
        /// Which source.
        source: Source,
        /// The type asked for.
        mime: String,
        /// Where to write it; closed when done.
        fd: OwnedFd,
    },
    /// The server's `source` is no longer a selection.
    Cancelled(Source),
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

    /// Bind the compositor's `ext_data_control_v1` for the seat; whether it
    /// has one. [`Event::Selection`], [`Event::Send`] and
    /// [`Event::Cancelled`] come only after this.
    fn bind_data_control(&mut self) -> bool;

    /// A new source offering `types`.
    ///
    /// # Errors
    ///
    /// The compositor refused.
    fn create_source(&mut self, types: &[&str]) -> io::Result<Source>;

    /// Let a source go.
    fn destroy_source(&mut self, source: Source);

    /// Make `source` the compositor's selection `which`, or clear it.
    fn set_selection(&mut self, which: Selection, source: Option<Source>);

    /// Ask for `offer` as `mime`, written to `fd`, and send the request at
    /// once: the compositor has its own copy of `fd` afterwards.
    ///
    /// # Errors
    ///
    /// The request could not be sent.
    fn receive(&mut self, offer: Offer, mime: &str, fd: BorrowedFd<'_>) -> io::Result<()>;

    /// Let an offer go.
    fn release_offer(&mut self, offer: Offer);
}
