//! The rootless topology: every top-level X window as a window of a host
//! window system, rather than as part of one screen the server owns.
//!
//! A rootless server's host is another display system that shows and
//! manages the server's windows one by one: a Wayland compositor today
//! (yserver's `ywayland`), and the native window systems of the macOS and
//! Windows ports later. What is the same for every such host lives here,
//! independent of how the host is spoken to:
//!
//! * which X windows become host windows, and what each is: a top-level of
//!   its own, with its title, its class, the window it is a dialog of and
//!   the sizes it may take ([`Topology`]); or a menu, tooltip or drop-down
//!   placed relative to the host window it belongs to ([`popups`]);
//! * the image each one shows, read from the window's redirect backing when
//!   it changed and the host is ready for another frame, and the rows that
//!   differ from the last one ([`RootlessRenderer`]);
//! * what the host asks of a window (a size, a close) carried out as a
//!   window manager's requests would be ([`carry_out`]), and the window
//!   manager requests clients send the root ([`move_resize_edges`]);
//! * the host's seat as X input: a position on a host window as a root
//!   position, the hit test kept to the top-level under the pointer, and
//!   the keyboard focus following the host's ([`seat`]).
//!
//! A host implements [`HostWindows`] -- make, change, draw and destroy its
//! windows -- and calls the topology from its backend's hooks. Nothing here
//! knows Wayland.
//!
//! Each top-level keeps the root position X gave it, wherever the host
//! shows it, as under Xwayland: the host does not say where it put a
//! window, and X hit-testing and popup placement then agree with each
//! other.

use std::io;

pub mod popups;
pub mod seat;
mod topology;

pub use topology::{
    HostRequest, SizeLimits, Topology, carry_out, is_toplevel, move_resize_edges, size_limits,
    take_over_root, title_and_class,
};

/// A band of rows that changed, as a rectangle: x, y, width, height.
pub type Damage = (u32, u32, u32, u32);

/// What the topology needs of the renderer: each top-level's image, by the
/// window's host XID, read through its redirect backing, which holds the
/// window's whole subtree once the server redirects the root's children
/// for itself ([`take_over_root`]). A rootless backend's renderer is also
/// its [`crate::backend::Backend`].
pub trait RootlessRenderer {
    /// A number that changes whenever anything is drawn into the window's
    /// image; `None` for a window the renderer does not know.
    fn image_version(&self, host_xid: u32) -> Option<u64>;

    /// The window's image, its `width` x `height` content with `border`
    /// pixels of its border around it (0 for none), each pixel four bytes in
    /// memory order B, G, R and alpha (unused below depth 32) -- an X
    /// `ZPixmap` at 32 bits per pixel -- read into `out`, which holds at
    /// least that many bytes; whether it was read.
    fn read_image_into(
        &mut self,
        host_xid: u32,
        width: u16,
        height: u16,
        border: u16,
        out: &mut [u8],
    ) -> bool;

    /// Which cursor is in effect, as the cursor's XID and a number that
    /// changes whenever its picture does; `None` for no cursor at all. Its
    /// picture is [`crate::backend::Backend::get_active_cursor_image`].
    fn cursor_key(&self) -> Option<(u32, u64)>;
}

/// What a top-level is made with: everything a host may want to know
/// before it first shows the window, where it decides whether the window
/// floats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToplevelSpec<S> {
    /// `_NET_WM_NAME`, or `WM_NAME`.
    pub title: String,
    /// `WM_CLASS`'s class, what a window rule matches.
    pub class: String,
    /// The window's size.
    pub size: (u32, u32),
    /// The host window it is a dialog of (`WM_TRANSIENT_FOR`).
    pub parent: Option<S>,
    /// The sizes it may take (`WM_NORMAL_HINTS`).
    pub limits: SizeLimits,
}

/// Where a popup hangs from its parent: the outer corner of the X window,
/// border and all, relative to the parent's content, and its size with
/// the border. The host must put it exactly there, never move it to fit
/// the screen: X placed it, and pointer input is translated by where X
/// thinks it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupPlacement {
    /// Relative to the parent's content.
    pub at: (i32, i32),
    /// Border included.
    pub size: (u16, u16),
}

/// A host window system, as the topology drives it. `Surface` names one of
/// its windows.
pub trait HostWindows {
    /// A host window.
    type Surface: Copy + Eq + std::fmt::Debug;

    /// Make a top-level window.
    ///
    /// # Errors
    ///
    /// The host refused.
    fn create_toplevel(&mut self, spec: &ToplevelSpec<Self::Surface>) -> io::Result<Self::Surface>;

    /// Make a popup on `parent`, placed exactly at `placement`.
    ///
    /// # Errors
    ///
    /// The host refused.
    fn create_popup(
        &mut self,
        parent: Self::Surface,
        placement: PopupPlacement,
    ) -> io::Result<Self::Surface>;

    /// Move a popup to `placement`.
    fn reposition_popup(&mut self, surface: Self::Surface, placement: PopupPlacement);

    /// Destroy a window or popup.
    fn destroy(&mut self, surface: Self::Surface);

    /// Retitle a top-level.
    fn set_title(&mut self, surface: Self::Surface, title: &str);

    /// Give a top-level another class.
    fn set_class(&mut self, surface: Self::Surface, class: &str);

    /// Make a top-level a dialog of `parent`, or of nothing.
    fn set_parent(&mut self, surface: Self::Surface, parent: Option<Self::Surface>);

    /// Give a top-level other size limits.
    fn set_size_limits(&mut self, surface: Self::Surface, limits: SizeLimits);

    /// Whether the window may be drawn: the host has told it its size.
    fn ready(&self, surface: Self::Surface) -> bool;

    /// Draw the window at `size`: `fill` writes the image into the host's
    /// buffer, `size.0` x `size.1` pixels of B, G, R, A bytes (`opaque`:
    /// alpha unused), and returns the bands of rows that changed (empty
    /// for all of them), or `None` to draw nothing after all. Returns
    /// whether a frame was drawn.
    ///
    /// # Errors
    ///
    /// The host refused.
    fn draw(
        &mut self,
        surface: Self::Surface,
        size: (u32, u32),
        opaque: bool,
        fill: &mut dyn FnMut(&mut [u8]) -> Option<Vec<Damage>>,
    ) -> io::Result<bool>;

    /// Ask to be told ([`Topology::frame_done`]) when the host is ready
    /// for the window's next frame.
    fn request_frame(&mut self, surface: Self::Surface);
}
