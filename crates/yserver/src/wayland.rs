//! The rootless Wayland backend: `ywayland`, an X server that is a client
//! of a Wayland compositor, each of whose top-level X windows is a window
//! of the compositor's.
//!
//! [`WaylandHost`] is the [`RootlessHost`] a
//! [`crate::rootless::RootlessBackend`] wraps the headless renderer for:
//! the root takes the size of the compositor's first screen (one RandR
//! output, `WAYLAND-1`), the keymap takes the compositor's layout, and
//! `yserver_core::rootless` decides what each X window becomes. The
//! compositor is spoken to through [`transport::Transport`] alone, which
//! [`toolkit`] implements on Ferrix's own client runtime; another Wayland
//! client library can take its place without a change here.
//!
//! * **Windows.** Each viewable, drawing, not override-redirect child of
//!   the root becomes an `xdg_toplevel`, its title `_NET_WM_NAME` or
//!   `WM_NAME`, its app id `WM_CLASS`'s class, `WM_TRANSIENT_FOR` its
//!   parent and `WM_NORMAL_HINTS` its least and greatest size. A window's
//!   image is read back and handed over as `wl_shm` when it changed and the
//!   compositor's last frame callback for it has come, so an X client that
//!   draws faster than the screen shows is read back once per frame, and an
//!   idle one not at all. The compositor's size for a window becomes a
//!   `ConfigureWindow` of it, its close `WM_DELETE_WINDOW`, and a press on
//!   a window's own title bar (`_NET_WM_MOVERESIZE`) the compositor's drag.
//! * **The clipboard** follows X's `CLIPBOARD` and `PRIMARY` both ways
//!   ([`clipboard`]).

use std::{io, os::fd::RawFd, time::Instant};

use yserver_core::{
    rootless::{Topology, carry_out, move_resize_edges},
    server::ServerState,
};
use yserver_protocol::x11::AtomId;

use crate::{
    kms::render::HostOutput,
    rootless::{Renderer, RootlessHost},
};

mod clipboard;
pub mod toolkit;
pub mod transport;

use transport::{Event, Surface, Transport};

/// The name the server announces as the window manager, unless
/// `YSERVER_WM_NAME` names another: the compositor manages the windows,
/// and Wayland does not say what it is called.
const WM_NAME: &str = "ywayland";

/// The EWMH hints the server honours as the window manager: the title is
/// carried over to the compositor, and a drag by a window's own title bar
/// or edge.
const WM_SUPPORTED: &[&str] = &[
    "_NET_SUPPORTING_WM_CHECK",
    "_NET_WM_NAME",
    "_NET_WM_MOVERESIZE",
];

/// The compositor, and what the server's windows are on it.
pub struct WaylandHost<T> {
    transport: T,
    topology: Topology<Surface>,
    /// X's selections and the compositor's clipboard ([`clipboard`]);
    /// `None` when the compositor has no `ext_data_control_v1`.
    clipboard: Option<clipboard::Clipboard>,
    wm_name: String,
}

impl<T: Transport> WaylandHost<T> {
    /// The host on `transport`, connected.
    pub fn new(mut transport: T) -> Self {
        let clipboard = transport
            .bind_data_control()
            .then(clipboard::Clipboard::default);
        if clipboard.is_none() {
            log::warn!("wayland: the compositor has no ext_data_control_v1; no clipboard");
        }
        let wm_name = std::env::var("YSERVER_WM_NAME").unwrap_or_else(|_| WM_NAME.to_owned());
        Self {
            transport,
            topology: Topology::new(),
            clipboard,
            wm_name,
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Frame(surface) => self.topology.frame_done(surface),
            Event::Configure {
                surface,
                width,
                height,
            } => self.topology.configured(surface, width, height),
            Event::Close(surface) => self.topology.close_requested(surface),
            event @ (Event::Selection { .. } | Event::Send { .. } | Event::Cancelled(_)) => {
                if let Some(clipboard) = self.clipboard.as_mut() {
                    clipboard.event(&mut self.transport, event);
                }
            }
        }
    }
}

impl<T: Transport> RootlessHost for WaylandHost<T> {
    fn fd(&self) -> RawFd {
        self.transport.fd()
    }

    fn screen(&self) -> Option<HostOutput> {
        let screen = self.transport.screen()?;
        // The compositor's rate, not a fixed 60: Chromium on X paces its
        // frames by the RandR mode's.
        let refresh_hz = (screen.refresh_mhz + 500) / 1000;
        Some(HostOutput {
            name: "WAYLAND-1".to_owned(),
            size: screen.size,
            refresh_hz: if refresh_hz == 0 { 60 } else { refresh_hz },
        })
    }

    fn keyboard_layout(&mut self) -> Option<(String, String)> {
        if self.transport.keyboard_layout().is_none() {
            // The keymap comes after the seat's keyboard is made, which the
            // first round trips only start.
            if let Err(error) = self.transport.roundtrip() {
                log::warn!("wayland: waiting for the keymap: {error}");
            }
        }
        self.transport.keyboard_layout()
    }

    fn window_manager(&self) -> (&str, &[&str]) {
        (&self.wm_name, WM_SUPPORTED)
    }

    fn dispatch(
        &mut self,
        _state: &mut ServerState,
        _renderer: &mut dyn Renderer,
    ) -> io::Result<()> {
        for event in self.transport.dispatch()? {
            self.handle(event);
        }
        Ok(())
    }

    fn sync(&mut self, state: &mut ServerState, renderer: &mut dyn Renderer) {
        // A press on a window's own title bar or edge becomes the
        // compositor's drag of its window, from the last press it sent.
        for request in std::mem::take(&mut state.server_wm_requests) {
            let yserver_core::server::ServerWmRequest::MoveResize { window, .. } = request;
            let (Some(edges), Some(surface)) = (
                move_resize_edges(request),
                self.topology.surface_holding(state, window),
            ) else {
                log::debug!("wayland: {request:?} not carried out");
                continue;
            };
            log::info!(
                "wayland: window 0x{:x} asks to be {} by the pointer",
                window.0,
                if edges == 0 { "moved" } else { "resized" }
            );
            self.transport.drag(surface, edges);
        }
        let requests = self.topology.sync(state, &mut self.transport, renderer);
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard.sync(&mut self.transport, state, renderer);
        }
        // What the compositor asked of the windows, which only the core,
        // with the state in hand, can carry out.
        carry_out(state, renderer, requests);
    }

    fn property_changed(&mut self, state: &ServerState, host_xid: u32, property: AtomId) {
        self.topology.property_changed(state, host_xid, property);
    }

    fn flush(&mut self) {
        if let Err(error) = self.transport.flush() {
            log::warn!("wayland: flush: {error}");
        }
    }

    fn next_wakeup(&self) -> Option<Instant> {
        // A paste between X and the compositor moves a little each
        // iteration, through pipes the loop does not poll.
        self.clipboard
            .as_ref()
            .filter(|clipboard| clipboard.busy())
            .map(|_| Instant::now() + std::time::Duration::from_millis(5))
    }
}
