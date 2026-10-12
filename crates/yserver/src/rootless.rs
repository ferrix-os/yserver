//! A rootless backend: the server's renderer, `KmsBackend` running headless,
//! wrapped so its top-level windows show as windows of a host window system
//! rather than on a screen of a card the server owns.
//!
//! [`RootlessBackend`] is a [`Backend`] like any other, which the core
//! loop drives through the trait alone. It passes every drawing, RENDER,
//! DRI3, Present and GLX method to the renderer it wraps (`delegate.rs`)
//! and takes the few hooks a host needs:
//!
//! * `poll_fds` adds the host's connection as
//!   [`BackendFdKind::HostDisplay`], whose readiness is
//!   `on_host_display_ready`: the host reads its events, the seat's among
//!   them, with the server's state in hand;
//! * `poll_deferred_input`, once per loop iteration, makes the server the
//!   stand-in for the window manager ([`take_over_root`]) and lets the host
//!   bring its windows in line with the X server's top-levels
//!   ([`RootlessHost::sync`], through `yserver_core::rootless`);
//! * `on_window_property_changed`, `before_block` and `next_wakeup` pass
//!   a window's renamed title, the end of an iteration and the host's own
//!   deadlines on.
//!
//! What a host is, is [`RootlessHost`]: the Wayland compositor of
//! `crate::wayland` today. The renderer stands the one RandR output for
//! the host's screen ([`HostOutput`]), which gives the root its size, and
//! takes the host's keyboard layout.

use std::{io, os::fd::RawFd, time::Instant};

use yserver_core::{
    backend::{Backend, BackendFdKind},
    rootless::{RootlessRenderer, take_over_root},
    server::ServerState,
};
use yserver_protocol::x11::AtomId;

use crate::kms::render::{HostOutput, KmsBackend};

mod delegate;

/// The renderer a host draws from and drives X through: one object, the
/// wrapped `KmsBackend`, as both.
pub trait Renderer: Backend + RootlessRenderer {}

impl<T: Backend + RootlessRenderer> Renderer for T {}

/// A host window system, as a [`RootlessBackend`] drives it.
pub trait RootlessHost {
    /// The connection the core loop polls for the host's events.
    fn fd(&self) -> RawFd;

    /// The host's screen, which the root takes; `None` while it has
    /// described none.
    fn screen(&self) -> Option<HostOutput>;

    /// The host's keyboard layout and variant, for the server's keymap;
    /// `None` when it said none.
    fn keyboard_layout(&mut self) -> Option<(String, String)>;

    /// The name the server announces as the window manager, and the EWMH
    /// hints it lists as honoured (`_NET_SUPPORTED`).
    fn window_manager(&self) -> (&str, &[&str]);

    /// Read what the host sent, without blocking, and act on it.
    ///
    /// # Errors
    ///
    /// The host closed the connection.
    fn dispatch(&mut self, state: &mut ServerState, renderer: &mut dyn Renderer) -> io::Result<()>;

    /// Bring the host's windows in line with the X server's top-levels and
    /// draw what changed. Once per loop iteration.
    fn sync(&mut self, state: &mut ServerState, renderer: &mut dyn Renderer);

    /// `property` was set or deleted on the window with host XID
    /// `host_xid`.
    fn property_changed(&mut self, state: &ServerState, host_xid: u32, property: AtomId);

    /// Send what is queued.
    fn flush(&mut self);

    /// When the loop must come round again for the host's sake, with
    /// nothing else to do.
    fn next_wakeup(&self) -> Option<Instant> {
        None
    }
}

/// The renderer, wrapped for a host.
pub struct RootlessBackend<H> {
    inner: KmsBackend,
    host: H,
    /// A client redirected the root's children before the server could,
    /// which was said once.
    redirect_refused: bool,
}

impl<H: RootlessHost> RootlessBackend<H> {
    /// Wrap `inner` for `host`: take the host's keyboard layout and its
    /// screen. Called once, before the topology is read.
    ///
    /// # Errors
    ///
    /// Resizing the virtual screen to the host's failing.
    pub fn new(mut inner: KmsBackend, mut host: H) -> io::Result<Self> {
        match host.keyboard_layout() {
            Some((layout, variant)) => {
                if inner.adopt_keyboard_layout(&layout, &variant) {
                    log::info!("rootless: the keyboard is the host's, {layout} {variant:?}");
                }
            }
            None => log::info!(
                "rootless: the host sent no keymap; the keyboard stays {}",
                inner.keyboard_layout()
            ),
        }
        match host.screen() {
            Some(screen) => {
                let (width, height) = screen.size;
                inner.set_host_output(screen)?;
                log::info!(
                    "rootless: the root window is the compositor's screen, {width}x{height}"
                );
            }
            None => log::warn!("rootless: the host described no screen; the root stays as it is"),
        }
        Ok(Self {
            inner,
            host,
            redirect_refused: false,
        })
    }

    /// The renderer.
    pub fn inner(&self) -> &KmsBackend {
        &self.inner
    }

    /// The renderer.
    pub fn inner_mut(&mut self) -> &mut KmsBackend {
        &mut self.inner
    }

    /// The host.
    pub fn host(&self) -> &H {
        &self.host
    }

    fn own_on_host_display_ready(&mut self, state: &mut ServerState) -> bool {
        match self.host.dispatch(state, &mut self.inner) {
            Ok(()) => {
                self.host.flush();
                true
            }
            Err(error) => {
                log::warn!("rootless: {error}");
                false
            }
        }
    }

    fn own_poll_deferred_input(&mut self, state: &mut ServerState) {
        self.inner.poll_deferred_input(state);
        let (name, supported) = self.host.window_manager();
        let redirected = take_over_root(state, &mut self.inner, name, supported);
        if !redirected && !self.redirect_refused {
            log::warn!(
                "rootless: a client redirects the root's children; subwindows will not show"
            );
        }
        self.redirect_refused = !redirected;
        self.host.sync(state, &mut self.inner);
        self.host.flush();
    }

    fn own_before_block(&mut self) {
        self.host.flush();
        self.inner.before_block();
    }

    fn own_next_wakeup(&self) -> Option<Instant> {
        match (self.inner.next_wakeup(), self.host.next_wakeup()) {
            (Some(inner), Some(host)) => Some(inner.min(host)),
            (inner, host) => inner.or(host),
        }
    }

    fn own_on_window_property_changed(
        &mut self,
        state: &ServerState,
        host_xid: u32,
        property: AtomId,
    ) {
        self.inner
            .on_window_property_changed(state, host_xid, property);
        self.host.property_changed(state, host_xid, property);
    }

    fn own_poll_fds(&self) -> Vec<(RawFd, BackendFdKind)> {
        let mut fds = self.inner.poll_fds();
        fds.push((self.host.fd(), BackendFdKind::HostDisplay));
        fds
    }
}

#[cfg(test)]
mod delegate_tests {
    /// Every method of the trait is in `delegate.rs`, delegated or taken
    /// by the rootless backend: a default method missing there would
    /// silently do nothing instead of what the renderer does.
    #[test]
    fn every_backend_method_is_delegated_or_overridden() {
        let trait_def = include_str!("../../yserver-core/src/backend/trait_def.rs");
        let delegate = include_str!("rootless/delegate.rs");
        let body = trait_def
            .split_once("pub trait Backend {")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap_or_default();
        let mut missing = Vec::new();
        for line in body.lines() {
            let Some(rest) = line.strip_prefix("    fn ") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !delegate.contains(&format!("    fn {name}("))
                && !delegate.contains(&format!("    fn {name}<"))
            {
                missing.push(name);
            }
        }
        assert!(missing.is_empty(), "not in delegate.rs: {missing:?}");
    }
}
