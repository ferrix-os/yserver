//! Top-level X windows as host windows, and what is drawn into them.

use std::collections::{HashMap, HashSet};

use log::{debug, info, warn};
use yserver_protocol::x11::{AtomId, ResourceId};

use super::{Damage, HostWindows, RootlessRenderer, ToplevelSpec};
use crate::{
    backend::Backend,
    core_loop::process_request::{
        announce_window_manager_for_server, close_window_for_server, configure_window_for_server,
        redirect_subwindows_for_server,
    },
    resources::{COMPOSITE_OVERLAY_WINDOW, MapState, ROOT_WINDOW, Window, WindowClass},
    server::{CompositeRedirectMode, ServerState, ServerWmRequest},
};

/// What the host asked of an X window, which the server carries out with
/// its state in hand ([`carry_out`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRequest {
    /// The host gave the window a size it does not have.
    Resize {
        /// The X window.
        window: ResourceId,
        /// Its new width.
        width: u16,
        /// Its new height.
        height: u16,
    },
    /// The host closed the window.
    Close {
        /// The X window.
        window: ResourceId,
    },
}

/// The least and greatest size a window may be given, in pixels; 0 is no
/// limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeLimits {
    /// The least size.
    pub min: (u32, u32),
    /// The greatest size.
    pub max: (u32, u32),
}

/// Where a host window is in drawing it: the frame it waits for and the
/// image it last showed.
#[derive(Debug, Default)]
struct Frames {
    /// A frame was asked for ([`HostWindows::request_frame`]) and has not
    /// come.
    pending: bool,
    /// The [`RootlessRenderer::image_version`] last drawn.
    shown: Option<u64>,
    /// The host configured the window since it was last drawn.
    configured: bool,
    /// A hash of each row of the image last handed over, which the next
    /// is compared with to say which rows changed.
    rows: Vec<u64>,
}

/// A top-level X window as a host window.
#[derive(Debug)]
struct Toplevel<S> {
    /// The X window.
    window: ResourceId,
    /// The host window.
    surface: S,
    /// What it was last given.
    title: String,
    /// What it was last given.
    class: String,
    /// Its frames.
    frames: Frames,
    /// The size the host last gave, not yet asked of the X window.
    resize: Option<(u32, u32)>,
    /// The host asked to close it, not yet passed on.
    close: bool,
    /// The host XID of the window it was last made a dialog of.
    parent: Option<u32>,
    /// The size limits it was last given.
    limits: SizeLimits,
}

/// The host windows the server's top-levels are, keyed by the X window's
/// host XID.
#[derive(Debug)]
pub struct Topology<S> {
    toplevels: HashMap<u32, Toplevel<S>>,
    /// Host XIDs whose title or class changed since the last sync.
    renamed: HashSet<u32>,
}

impl<S> Default for Topology<S> {
    fn default() -> Self {
        Self {
            toplevels: HashMap::new(),
            renamed: HashSet::new(),
        }
    }
}

impl<S: Copy + Eq + std::fmt::Debug> Topology<S> {
    /// No windows yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The host gave the window `surface` a size.
    pub fn configured(&mut self, surface: S, width: u32, height: u32) {
        if let Some(toplevel) = self.by_surface(surface) {
            toplevel.resize = Some((width, height));
        }
        if let Some(frames) = self.frames_of(surface) {
            frames.configured = true;
        }
    }

    /// The host asked to close the window `surface`.
    pub fn close_requested(&mut self, surface: S) {
        if let Some(toplevel) = self.by_surface(surface) {
            toplevel.close = true;
        }
    }

    /// The host is ready for another frame of `surface`.
    pub fn frame_done(&mut self, surface: S) {
        if let Some(frames) = self.frames_of(surface) {
            frames.pending = false;
        }
    }

    /// A window's title or class may have changed: `property` was set or
    /// deleted on the window with host XID `host_xid`.
    pub fn property_changed(&mut self, state: &ServerState, host_xid: u32, property: AtomId) {
        if is_name(state, property) {
            let _ = self.renamed.insert(host_xid);
        }
    }

    /// The host XID of the X window the host window `surface` shows.
    #[must_use]
    pub fn host_xid_of(&self, surface: S) -> Option<u32> {
        self.toplevels
            .iter()
            .find(|(_, toplevel)| toplevel.surface == surface)
            .map(|(&host_xid, _)| host_xid)
    }

    /// The X window the host window `surface` shows, if it still is a child
    /// of the root.
    #[must_use]
    pub fn window_of(&self, state: &ServerState, surface: S) -> Option<ResourceId> {
        let host_xid = self.host_xid_of(surface)?;
        root_child_with_host_xid(state, host_xid)
    }

    /// The host window that shows `window`, or the top-level it is inside:
    /// what a window manager request about `window` acts on.
    #[must_use]
    pub fn surface_holding(&self, state: &ServerState, window: ResourceId) -> Option<S> {
        let mut current = window;
        loop {
            if let Some(toplevel) = self
                .toplevels
                .values()
                .find(|toplevel| toplevel.window == current)
            {
                return Some(toplevel.surface);
            }
            match state.resources.parent_of(current) {
                Some(parent) if parent != current && parent != ROOT_WINDOW => current = parent,
                _ => return None,
            }
        }
    }

    /// Bring the host's windows in line with the X server's top-levels, and
    /// draw each one whose image changed and whose last frame the host has
    /// shown. Once per loop iteration. Returns what the host asked of the
    /// windows since, for [`carry_out`].
    pub fn sync<H: HostWindows<Surface = S>>(
        &mut self,
        state: &ServerState,
        host: &mut H,
        renderer: &mut dyn RootlessRenderer,
    ) -> Vec<HostRequest> {
        let mut requests = Vec::new();
        let mut wanted: Vec<(u32, &Window)> = state
            .resources
            .children(ROOT_WINDOW)
            .iter()
            .filter_map(|&child| {
                let window = state.resources.window(child)?;
                let host_xid = window.host_xid?.as_raw();
                is_toplevel(child, window).then_some((host_xid, window))
            })
            .collect();
        // Dialogs after the windows they belong to, so that a dialog is
        // made with its parent in hand: a host may decide when it first
        // shows a window whether it floats.
        wanted.sort_by_key(|(_, window)| transient_for(state, window).is_some());

        let kept: HashSet<u32> = wanted.iter().map(|&(host_xid, _)| host_xid).collect();

        self.toplevels.retain(|host_xid, toplevel| {
            let keep = kept.contains(host_xid);
            if !keep {
                info!("rootless: window 0x{host_xid:x} is gone from the host");
                host.destroy(toplevel.surface);
            }
            keep
        });

        for &(host_xid, window) in &wanted {
            // The window it is a dialog of, when that is a host window
            // already.
            let parent = transient_for(state, window)
                .filter(|parent| *parent != host_xid && self.toplevels.contains_key(parent));
            let parent_surface = parent
                .and_then(|parent| self.toplevels.get(&parent))
                .map(|parent| parent.surface);
            let limits = size_limits_of(state, window);
            let toplevel = match self.toplevels.entry(host_xid) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let (title, class) = title_and_class(state, window);
                    let spec = ToplevelSpec {
                        title: title.clone(),
                        class: class.clone(),
                        size: (u32::from(window.width), u32::from(window.height)),
                        parent: parent_surface,
                        limits,
                    };
                    match host.create_toplevel(&spec) {
                        Ok(surface) => {
                            info!(
                                "rootless: window 0x{host_xid:x} ({}x{}) is the host's window \
                                 {surface:?}, {title:?} of {class:?}",
                                window.width, window.height,
                            );
                            entry.insert(Toplevel {
                                window: window.id,
                                surface,
                                title,
                                class,
                                frames: Frames::default(),
                                resize: None,
                                close: false,
                                parent,
                                limits,
                            })
                        }
                        Err(error) => {
                            warn!("rootless: no host window for 0x{host_xid:x}: {error}");
                            continue;
                        }
                    }
                }
            };

            // A window made a dialog, or no longer one, after it mapped.
            if parent != toplevel.parent {
                toplevel.parent = parent;
                host.set_parent(toplevel.surface, parent_surface);
            }

            // Size hints set or changed after it mapped.
            if limits != toplevel.limits {
                toplevel.limits = limits;
                host.set_size_limits(toplevel.surface, limits);
            }

            if self.renamed.contains(&host_xid) {
                let (title, class) = title_and_class(state, window);
                if title != toplevel.title {
                    host.set_title(toplevel.surface, &title);
                    toplevel.title = title;
                }
                if class != toplevel.class {
                    host.set_class(toplevel.surface, &class);
                    toplevel.class = class;
                }
            }

            // What the host asked, carried out by the server after this.
            if let Some((width, height)) = toplevel.resize.take()
                && (width, height) != (u32::from(window.width), u32::from(window.height))
            {
                requests.push(HostRequest::Resize {
                    window: window.id,
                    width: u16::try_from(width).unwrap_or(u16::MAX),
                    height: u16::try_from(height).unwrap_or(u16::MAX),
                });
            }
            if std::mem::take(&mut toplevel.close) {
                info!("rootless: the host closes window 0x{host_xid:x}");
                requests.push(HostRequest::Close { window: window.id });
            }

            show(
                host,
                renderer,
                toplevel.surface,
                (host_xid, window),
                &mut toplevel.frames,
            );
        }
        self.renamed.clear();
        requests
    }

    fn by_surface(&mut self, surface: S) -> Option<&mut Toplevel<S>> {
        self.toplevels
            .values_mut()
            .find(|toplevel| toplevel.surface == surface)
    }

    /// The frames of the window `surface` is.
    fn frames_of(&mut self, surface: S) -> Option<&mut Frames> {
        self.toplevels
            .values_mut()
            .find(|toplevel| toplevel.surface == surface)
            .map(|toplevel| &mut toplevel.frames)
    }
}

/// Make the server the stand-in for the window manager a rootless server
/// has no X client for, once per loop iteration (each step is cheap when
/// already done, and puts back what a server reset took):
///
/// * redirect the root's children for the server itself, so each
///   top-level's image, its subwindows included, is one backing
///   ([`RootlessRenderer::read_image_into`]), as Xwayland does for its
///   `serverClient`;
/// * announce a window manager named `name` over EWMH, listing
///   `supported`, so clients that look for one find it.
///
/// Returns whether the redirect is the server's: `false` when a client
/// redirected the root's children first, which is left alone; windows
/// still show then, without their subwindows.
pub fn take_over_root(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    name: &str,
    supported: &[&str],
) -> bool {
    let redirected =
        redirect_subwindows_for_server(state, backend, ROOT_WINDOW, CompositeRedirectMode::Manual);
    if let Err(error) = announce_window_manager_for_server(state, backend, name, supported) {
        warn!("rootless: announcing the window manager: {error}");
    }
    redirected
}

/// Carry out what the host asked of the windows ([`Topology::sync`]), as a
/// window manager's requests would be.
pub fn carry_out(state: &mut ServerState, backend: &mut dyn Backend, requests: Vec<HostRequest>) {
    for request in requests {
        let done = match request {
            HostRequest::Resize {
                window,
                width,
                height,
            } => configure_window_for_server(state, backend, window, width, height),
            HostRequest::Close { window } => close_window_for_server(state, backend, window),
        };
        if let Err(error) = done {
            warn!("rootless: {request:?}: {error}");
        }
    }
}

/// The edges a `_NET_WM_MOVERESIZE` (EWMH 1.5) drags, for a host's
/// interactive move or resize: `Some(0)` for a move, else bits top 1,
/// bottom 2, left 4 and right 8 (which are also `xdg_toplevel`'s
/// `resize_edge` values); `None` for one that is not a drag by the pointer
/// (the keyboard forms and a cancel, which a host's drag needs no word of:
/// it follows the pointer and ends with the button).
#[must_use]
pub fn move_resize_edges(request: ServerWmRequest) -> Option<u32> {
    let ServerWmRequest::MoveResize { direction, .. } = request;
    // EWMH numbers the edges clockwise from the top left.
    Some(match direction {
        0 => 5,  // top left
        1 => 1,  // top
        2 => 9,  // top right
        3 => 8,  // right
        4 => 10, // bottom right
        5 => 2,  // bottom
        6 => 6,  // bottom left
        7 => 4,  // left
        8 => 0,  // move
        _ => return None,
    })
}

/// Hand the host `window`'s image when it changed and its last frame has
/// been shown, at the window's own size: until the X window has taken a
/// size the host asked for, it shows at the one it has.
///
/// The image is read straight into the host's buffer, and the host is told
/// only the band of rows that differ from the image before, found by a hash
/// of each row ([`Frames::rows`]): what X drew is not needed for that, so no
/// drawing path can be missed.
fn show<H: HostWindows>(
    host: &mut H,
    renderer: &mut dyn RootlessRenderer,
    surface: H::Surface,
    (host_xid, window): (u32, &Window),
    frames: &mut Frames,
) {
    if frames.pending || !host.ready(surface) {
        return;
    }
    let version = renderer.image_version(host_xid);
    if version.is_some() && version == frames.shown && !frames.configured {
        return;
    }
    // A top-level's border is not shown, as a window manager's frame would
    // take its place.
    let (width, height) = (window.width, window.height);
    let opaque = window.depth != 32;
    let size = (u32::from(width), u32::from(height));
    // Whether the host has this size's image already: a configure since,
    // or a first frame, or another size, is all of it changed.
    let whole =
        frames.configured || frames.shown.is_none() || frames.rows.len() != usize::from(height);
    let rows = &mut frames.rows;
    let mut fill = |out: &mut [u8]| {
        if !renderer.read_image_into(host_xid, width, height, 0, out) {
            return None;
        }
        Some(changed_rows(out, size, rows, whole))
    };
    match host.draw(surface, size, opaque, &mut fill) {
        Ok(true) => {
            host.request_frame(surface);
            frames.pending = true;
            frames.shown = version;
            frames.configured = false;
        }
        Ok(false) => {}
        Err(error) => warn!("rootless: drawing 0x{host_xid:x}: {error}"),
    }
}

/// The damage of a new image `out` of `size` against the row hashes of the
/// one before, which `rows` is brought up to date with: none (all of it)
/// when `whole`, else the band from the first row that changed to the last,
/// or one row when none did -- a version moved by a draw that drew the same
/// pixels, where one row keeps the frame meaningful.
fn changed_rows(out: &[u8], size: (u32, u32), rows: &mut Vec<u64>, whole: bool) -> Vec<Damage> {
    let stride = size.0 as usize * 4;
    let hashes = out
        .chunks_exact(stride.max(1))
        .take(size.1 as usize)
        .map(row_hash);
    if whole {
        rows.clear();
        rows.extend(hashes);
        return Vec::new();
    }
    let mut band: Option<(u32, u32)> = None;
    for (y, hash) in hashes.enumerate() {
        if rows.get(y) != Some(&hash) {
            if let Some(row) = rows.get_mut(y) {
                *row = hash;
            }
            let y = u32::try_from(y).unwrap_or(u32::MAX);
            band = Some(band.map_or((y, y), |(first, _)| (first, y)));
        }
    }
    let (first, last) = band.unwrap_or((0, 0));
    vec![(0, first, size.0, last - first + 1)]
}

/// A row's hash, for [`changed_rows`]'s comparison with the image before:
/// FxHash's step over its 8-byte words, which is as fast as reading them.
fn row_hash(row: &[u8]) -> u64 {
    let mut hash: u64 = 0;
    let mut words = row.chunks_exact(8);
    for word in &mut words {
        let word = u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
        hash = (hash.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    for &byte in words.remainder() {
        hash = (hash.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    hash
}

/// Whether a child of the root is shown as a top-level host window:
/// viewable, drawing, and not override-redirect (a menu or a tooltip, which
/// places itself).
#[must_use]
pub fn is_toplevel(id: ResourceId, window: &Window) -> bool {
    id != COMPOSITE_OVERLAY_WINDOW
        && window.map_state == MapState::Viewable
        && window.class != WindowClass::InputOnly
        && !window.override_redirect
        && window.width > 0
        && window.height > 0
}

/// The child of the root whose host XID is `host_xid`.
fn root_child_with_host_xid(state: &ServerState, host_xid: u32) -> Option<ResourceId> {
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

/// The host XID of the window `window` is a dialog of: `WM_TRANSIENT_FOR`
/// (ICCCM 4.1.2.6), when it names a window that has one.
fn transient_for(state: &ServerState, window: &Window) -> Option<u32> {
    let atom = state.atoms.id_for("WM_TRANSIENT_FOR")?;
    let data = window.properties.get(&atom)?.data.get(0..4)?;
    let id = u32::from_le_bytes(data.try_into().ok()?);
    let parent = state.resources.window(ResourceId(id))?;
    (parent.id != window.id)
        .then_some(parent.host_xid)
        .flatten()
        .map(|host| host.as_raw())
}

/// `window`'s [`SizeLimits`] from `WM_NORMAL_HINTS`.
fn size_limits_of(state: &ServerState, window: &Window) -> SizeLimits {
    state
        .atoms
        .id_for("WM_NORMAL_HINTS")
        .and_then(|atom| window.properties.get(&atom))
        .map(|value| size_limits(&value.data))
        .unwrap_or_default()
}

/// The least and greatest size in a `WM_SIZE_HINTS` value (ICCCM 4.1.2.3):
/// 32-bit words, `flags` first, `min_width` and `min_height` the sixth and
/// seventh, `max_width` and `max_height` the eighth and ninth, each pair
/// meant only when its flag (`PMinSize`, `PMaxSize`) is set. A window whose
/// least size is its greatest is one of a fixed size, which a tiling host
/// floats.
#[must_use]
pub fn size_limits(data: &[u8]) -> SizeLimits {
    const P_MIN_SIZE: u32 = 1 << 4;
    const P_MAX_SIZE: u32 = 1 << 5;
    let word = |index: usize| {
        data.get(index * 4..index * 4 + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    };
    // A size is a positive `INT32`; anything else is no limit.
    let size = |index: usize| {
        word(index)
            .filter(|&value| i32::try_from(value).is_ok())
            .unwrap_or(0)
    };
    let pair = |flag: u32, first: usize| match word(0) {
        Some(flags) if flags & flag != 0 && word(first + 1).is_some() => {
            (size(first), size(first + 1))
        }
        _ => (0, 0),
    };
    SizeLimits {
        min: pair(P_MIN_SIZE, 5),
        max: pair(P_MAX_SIZE, 7),
    }
}

/// Whether `property` is one [`title_and_class`] reads.
fn is_name(state: &ServerState, property: AtomId) -> bool {
    ["_NET_WM_NAME", "WM_NAME", "WM_CLASS"]
        .iter()
        .any(|name| state.atoms.id_for(name) == Some(property))
}

/// A window's title and class: `_NET_WM_NAME` (UTF-8) or else `WM_NAME`
/// (Latin-1), and `WM_CLASS`'s class. Missing ones are empty.
#[must_use]
pub fn title_and_class(state: &ServerState, window: &Window) -> (String, String) {
    let property = |name: &str| {
        let atom = state.atoms.id_for(name)?;
        window
            .properties
            .get(&atom)
            .map(|value| value.data.as_slice())
    };
    let title = property("_NET_WM_NAME")
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .or_else(|| property("WM_NAME").map(latin1))
        .unwrap_or_default();
    let class = property("WM_CLASS").map(class).unwrap_or_default();
    debug!("rootless: 0x{:x} is {title:?} of {class:?}", window.id.0);
    (title, class)
}

/// `WM_CLASS`'s class: the second of the two strings it holds, each ended
/// by a NUL, or the only one where a program set just one.
fn class(bytes: &[u8]) -> String {
    let mut strings = bytes.split(|&byte| byte == 0);
    let instance = strings.next().unwrap_or_default();
    let class = strings.next().filter(|class| !class.is_empty());
    latin1(class.unwrap_or(instance))
}

/// ICCCM `STRING` text, which is Latin-1.
fn latin1(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| char::from(byte))
        .collect()
}

#[cfg(test)]
mod tests;
