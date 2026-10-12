//! Override-redirect X windows -- menus, drop-downs, tooltips -- as popups
//! of the host window they belong to.
//!
//! An override-redirect window places itself, in root coordinates, and a
//! host window system does not let a client place a window on its screen
//! (Wayland does not), or the server does not know where the host showed
//! the window it belongs to. So each becomes a popup on that host window,
//! placed at its own X position relative to it and never moved by the
//! host ([`PopupPlacement`]): where X put it relative to its parent is
//! where it shows, which is also where the seat's pointer translation
//! ([`super::seat`]) expects it to be.
//!
//! **The parent**, in order: the window `WM_TRANSIENT_FOR` names; the
//! window the pointer is on, which is where the click that opened a menu
//! was, and the parent menu for a submenu; the topmost shown window that
//! holds the popup's position; the topmost top-level. A popup whose parent
//! goes goes first, as a host such as `xdg_popup` requires.
//!
//! A popup the host takes away ([`super::Topology::popup_dismissed`]) is not
//! made again until its X window unmaps: the X client still thinks its menu
//! is up, and remaking it would put back what the person just dismissed.

use std::collections::HashSet;

use log::{debug, info, warn};
use yserver_protocol::x11::ResourceId;

use super::{
    HostWindows, PopupPlacement, RootlessRenderer, Topology,
    topology::{Frames, root_child_with_host_xid, show, transient_for},
};
use crate::{
    resources::{COMPOSITE_OVERLAY_WINDOW, MapState, ROOT_WINDOW, Window, WindowClass},
    server::ServerState,
};

/// An override-redirect X window as a host popup.
#[derive(Debug)]
struct Popup<S> {
    /// The X window's host XID.
    host_xid: u32,
    /// The host's popup.
    surface: S,
    /// The host XID of the window or popup it hangs from.
    parent: u32,
    /// Where it hangs from.
    placed: PopupPlacement,
    /// Its frames.
    frames: Frames,
}

/// The popups, in the order they were made, which a child popup is after
/// its parent in.
#[derive(Debug)]
pub(super) struct Popups<S> {
    popups: Vec<Popup<S>>,
    /// Host XIDs of popups the host took away, not made again until their
    /// X window unmaps.
    dismissed: HashSet<u32>,
}

impl<S> Default for Popups<S> {
    fn default() -> Self {
        Self {
            popups: Vec::new(),
            dismissed: HashSet::new(),
        }
    }
}

/// A shown top-level, as [`Popups::show`] is told of it: its host XID,
/// its host window, and whether the host has drawn it once.
pub(super) type Shown<S> = (u32, S, bool);

impl<S: Copy + Eq + std::fmt::Debug> Topology<S> {
    /// The host took the popup `surface` away.
    pub fn popup_dismissed<H: HostWindows<Surface = S>>(&mut self, host: &mut H, surface: S) {
        let popups = &mut self.popups;
        let Some(at) = popups
            .popups
            .iter()
            .position(|popup| popup.surface == surface)
        else {
            return;
        };
        let popup = popups.popups.remove(at);
        info!("rootless: the host took popup 0x{:x} away", popup.host_xid);
        let _ = popups.dismissed.insert(popup.host_xid);
        host.destroy(popup.surface);
    }

    /// The host window the pointer is on, as the host's seat says it.
    pub fn set_pointer_surface(&mut self, surface: Option<S>) {
        self.pointer = surface;
    }
}

impl<S: Copy + Eq + std::fmt::Debug> Popups<S> {
    pub(super) fn host_xid_of(&self, surface: S) -> Option<u32> {
        self.popups
            .iter()
            .find(|popup| popup.surface == surface)
            .map(|popup| popup.host_xid)
    }

    pub(super) fn frames_of(&mut self, surface: S) -> Option<&mut Frames> {
        self.popups
            .iter_mut()
            .find(|popup| popup.surface == surface)
            .map(|popup| &mut popup.frames)
    }

    /// Destroy the popups that go: their X window is gone or unmapped, or
    /// what they hang from goes (`kept`, the top-levels that stay, and the
    /// popups kept before them). Children before parents, which is the
    /// reverse of the order they were made in.
    pub(super) fn retire<H: HostWindows<Surface = S>>(
        &mut self,
        state: &ServerState,
        host: &mut H,
        kept: &HashSet<u32>,
    ) {
        let wanted: HashSet<u32> = menus(state).iter().map(|&(host_xid, _)| host_xid).collect();
        self.dismissed.retain(|host_xid| wanted.contains(host_xid));
        let mut staying: HashSet<u32> = HashSet::new();
        let keep: Vec<bool> = self
            .popups
            .iter()
            .map(|popup| {
                let stays = wanted.contains(&popup.host_xid)
                    && (kept.contains(&popup.parent) || staying.contains(&popup.parent));
                if stays {
                    let _ = staying.insert(popup.host_xid);
                }
                stays
            })
            .collect();
        for (at, stays) in keep.iter().enumerate().rev() {
            if !stays {
                let popup = self.popups.remove(at);
                info!(
                    "rootless: popup 0x{:x} is gone from the host",
                    popup.host_xid
                );
                host.destroy(popup.surface);
            }
        }
    }

    /// Make the popups there are not yet, move the ones whose X window
    /// moved or was resized, and draw each image that changed. `toplevels`
    /// are the shown top-levels, `pointer` the host window the pointer is
    /// on.
    pub(super) fn show<H: HostWindows<Surface = S>>(
        &mut self,
        state: &ServerState,
        host: &mut H,
        renderer: &mut dyn RootlessRenderer,
        toplevels: &[Shown<S>],
        pointer: Option<S>,
    ) {
        for (host_xid, window) in menus(state) {
            if self.dismissed.contains(&host_xid) {
                continue;
            }
            match self
                .popups
                .iter()
                .position(|popup| popup.host_xid == host_xid)
            {
                Some(at) => self.follow(state, host, at, window),
                None => self.open(state, host, toplevels, pointer, host_xid, window),
            }
            if let Some(popup) = self
                .popups
                .iter_mut()
                .find(|popup| popup.host_xid == host_xid)
            {
                show(
                    host,
                    renderer,
                    popup.surface,
                    (host_xid, window),
                    &mut popup.frames,
                    true,
                );
            }
        }
    }

    /// Make `window` a popup on the window it belongs to.
    fn open<H: HostWindows<Surface = S>>(
        &mut self,
        state: &ServerState,
        host: &mut H,
        toplevels: &[Shown<S>],
        pointer: Option<S>,
        host_xid: u32,
        window: &Window,
    ) {
        let Some(parent) = self.parent(state, toplevels, pointer, host_xid, window) else {
            debug!("rootless: popup 0x{host_xid:x} has no window to hang from yet");
            return;
        };
        let (Some(parent_surface), Some(placed)) = (
            self.surface_of(toplevels, parent),
            placement(state, parent, window),
        ) else {
            return;
        };
        match host.create_popup(parent_surface, placed) {
            Ok(surface) => {
                info!(
                    "rootless: override-redirect window 0x{host_xid:x} ({}x{}) is a popup on \
                     0x{parent:x} at {:?}",
                    placed.size.0, placed.size.1, placed.at,
                );
                self.popups.push(Popup {
                    host_xid,
                    surface,
                    parent,
                    placed,
                    frames: Frames::default(),
                });
            }
            Err(error) => warn!("rootless: no popup for 0x{host_xid:x}: {error}"),
        }
    }

    /// Move the popup at `at` where its X window now is, if it moved or was
    /// resized.
    fn follow<H: HostWindows<Surface = S>>(
        &mut self,
        state: &ServerState,
        host: &mut H,
        at: usize,
        window: &Window,
    ) {
        let Some(popup) = self.popups.get_mut(at) else {
            return;
        };
        let Some(placed) = placement(state, popup.parent, window) else {
            return;
        };
        if placed == popup.placed {
            return;
        }
        host.reposition_popup(popup.surface, placed);
        popup.placed = placed;
        // A popup the host remade under the same name, where it could not
        // move it, is configured afresh.
        popup.frames = Frames::default();
    }

    /// The window or popup `window` hangs from, by host XID.
    fn parent(
        &self,
        state: &ServerState,
        toplevels: &[Shown<S>],
        pointer: Option<S>,
        host_xid: u32,
        window: &Window,
    ) -> Option<u32> {
        // Only what the host has drawn once: a popup's parent must be shown
        // before it.
        let shown = |candidate: u32| {
            candidate != host_xid
                && (toplevels
                    .iter()
                    .any(|&(xid, _, drawn)| xid == candidate && drawn)
                    || self
                        .popups
                        .iter()
                        .any(|popup| popup.host_xid == candidate && popup.frames.shown.is_some()))
        };
        if let Some(parent) = transient_for(state, window).filter(|&parent| shown(parent)) {
            return Some(parent);
        }
        if let Some(parent) = pointer
            .and_then(|surface| {
                toplevels
                    .iter()
                    .find(|&&(_, s, _)| s == surface)
                    .map(|&(xid, _, _)| xid)
                    .or_else(|| self.host_xid_of(surface))
            })
            .filter(|&parent| shown(parent))
        {
            return Some(parent);
        }
        let (x, y) = state.resources.window_absolute_position(window.id);
        let holds = |candidate: u32| {
            root_child_with_host_xid(state, candidate).is_some_and(|id| {
                let Some(found) = state.resources.window(id) else {
                    return false;
                };
                let (left, top) = state.resources.window_absolute_position(id);
                x >= left
                    && y >= top
                    && x < left + i32::from(found.width)
                    && y < top + i32::from(found.height)
            })
        };
        let popups = self.popups.iter().rev().map(|popup| popup.host_xid);
        let stacked = toplevels_top_down(state, toplevels);
        popups
            .chain(stacked.iter().copied())
            .find(|&candidate| shown(candidate) && holds(candidate))
            .or_else(|| stacked.into_iter().find(|&candidate| shown(candidate)))
    }

    /// The host window for a top-level or popup, by host XID.
    fn surface_of(&self, toplevels: &[Shown<S>], host_xid: u32) -> Option<S> {
        toplevels
            .iter()
            .find(|&&(xid, _, _)| xid == host_xid)
            .map(|&(_, surface, _)| surface)
            .or_else(|| {
                self.popups
                    .iter()
                    .find(|popup| popup.host_xid == host_xid)
                    .map(|popup| popup.surface)
            })
    }
}

/// Where `window` hangs from on `parent` (a host XID): its outer corner,
/// border and all, relative to the parent's content, both in root
/// coordinates, and its size with the border.
fn placement(state: &ServerState, parent: u32, window: &Window) -> Option<PopupPlacement> {
    let parent = root_child_with_host_xid(state, parent)?;
    let (parent_x, parent_y) = state.resources.window_absolute_position(parent);
    let (x, y) = state.resources.window_absolute_position(window.id);
    let border = window.border_width;
    Some(PopupPlacement {
        at: (
            x - i32::from(border) - parent_x,
            y - i32::from(border) - parent_y,
        ),
        size: (
            window.width.saturating_add(border.saturating_mul(2)),
            window.height.saturating_add(border.saturating_mul(2)),
        ),
    })
}

/// The shown top-levels in X's stacking order, topmost first.
fn toplevels_top_down<S>(state: &ServerState, toplevels: &[Shown<S>]) -> Vec<u32> {
    state
        .resources
        .children(ROOT_WINDOW)
        .iter()
        .rev()
        .filter_map(|&child| {
            let host_xid = state.resources.window(child)?.host_xid?.as_raw();
            toplevels
                .iter()
                .any(|&(xid, _, _)| xid == host_xid)
                .then_some(host_xid)
        })
        .collect()
}

/// The children of the root shown as popups, bottom to top: viewable,
/// drawing and override-redirect.
fn menus(state: &ServerState) -> Vec<(u32, &Window)> {
    state
        .resources
        .children(ROOT_WINDOW)
        .iter()
        .filter_map(|&child| {
            let window = state.resources.window(child)?;
            let host_xid = window.host_xid?.as_raw();
            is_popup(child, window).then_some((host_xid, window))
        })
        .collect()
}

/// Whether a child of the root is shown as a popup.
fn is_popup(id: ResourceId, window: &Window) -> bool {
    id != COMPOSITE_OVERLAY_WINDOW
        && window.map_state == MapState::Viewable
        && window.class != WindowClass::InputOnly
        && window.override_redirect
        && window.width > 0
        && window.height > 0
}
