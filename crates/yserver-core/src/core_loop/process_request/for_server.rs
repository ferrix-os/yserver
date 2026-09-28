//! Requests the server makes for itself, as a client or a window manager
//! would make them: what a rootless server needs when another display
//! system shows its top-level windows (yserver's Wayland backend).

use super::*;

/// Redirect every child of `parent` for the server itself: the record a
/// client's `RedirectSubwindows(parent, mode)` would make, owned by
/// `SERVER_OWNER`, so no disconnect tears it down. Children that exist now
/// get their backings at once; later ones get theirs as they map, through
/// the hooks a client's redirect goes through.
///
/// A rootless server whose top-level windows another display system shows
/// (yserver's Wayland backend, where each is a Wayland window) needs each
/// top-level's whole subtree in one image it can read back, and that is what
/// a redirect backing holds. Xwayland redirects its top-levels for its own
/// `serverClient` for the same reason.
///
/// Cheap when the record is there already, so it may be called on every
/// loop iteration, which also puts it back after a server reset. Returns
/// whether the server's record is in place: `false` when a client
/// redirected `parent`'s children first, which is left alone, or when the
/// backend cannot redirect.
pub fn redirect_subwindows_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    parent: ResourceId,
    mode: crate::server::CompositeRedirectMode,
) -> bool {
    let owner = crate::resources::SERVER_OWNER;
    let records = state.composite_redirects.subwindows_records(parent);
    if !records.is_empty() {
        return records.iter().any(|r| r.owner == owner);
    }
    if !backend.supports_redirect_activation() {
        return false;
    }
    // As a client's RedirectSubwindows: top first, and no record for the
    // overlay window.
    let targets: Vec<ResourceId> = state
        .resources
        .children(parent)
        .iter()
        .rev()
        .copied()
        .filter(|child| *child != COMPOSITE_OVERLAY_WINDOW)
        .collect();
    let before: Vec<_> = targets
        .iter()
        .map(|t| state.composite_redirects.window_mode(*t))
        .collect();
    let record = crate::server::RedirectRecord { mode, owner };
    if state
        .composite_redirects
        .redirect_subwindows(parent, &targets, record)
        .is_err()
    {
        return false;
    }
    debug!(
        "the server redirects the children of 0x{:x} ({mode:?})",
        parent.0
    );
    for (target, before) in targets.into_iter().zip(before) {
        sync_redirect_backing(state, backend, None, target, before);
    }
    true
}

/// The server's own SetInputFocus, for a backend whose host decides the
/// focus: a rootless server gives its top-level the focus when the host's
/// keyboard enters that window, as the window manager would, and takes it
/// away (`None`) when the keyboard leaves. `window` is `None` for None; a
/// window that is gone or not viewable is refused, as the request would be.
/// The focus reverts to PointerRoot if the window goes.
///
/// Returns whether the focus is now where it was asked to be.
pub fn set_input_focus_for_server(state: &mut ServerState, window: Option<ResourceId>) -> bool {
    let to_raw = match window {
        None => 0,
        Some(window) => {
            let viewable = state
                .resources
                .window(window)
                .is_some_and(|w| w.map_state == crate::resources::MapState::Viewable);
            if !viewable {
                return false;
            }
            window.0
        }
    };
    let time = state
        .timestamp_now()
        .max(state.xi1_last_input_time)
        .max(state.core_focus.time);
    let revert_to = if to_raw == 0 { 0 } else { 1 };
    apply_core_focus(state, to_raw, revert_to, time);
    true
}

/// Resize a window for the server itself, as a window manager's
/// `ConfigureWindow` would: the window, its backing and its children follow,
/// and its client gets the `ConfigureNotify`. A client holding
/// `SubstructureRedirect` on the window's parent gets a `ConfigureRequest`
/// instead, as it would from any other client.
///
/// A rootless server uses it when the display system that shows its
/// top-levels gives one a size: a Wayland compositor's
/// `xdg_toplevel.configure`, for yserver's Wayland backend. A size of zero,
/// which X refuses, does nothing.
///
/// # Errors
///
/// As the request's handler.
pub fn configure_window_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    width: u16,
    height: u16,
) -> io::Result<()> {
    if width == 0 || height == 0 {
        return Ok(());
    }
    // CWWidth | CWHeight, and their values in that order.
    const CW_WIDTH_HEIGHT: u16 = 0x0004 | 0x0008;
    let mut body = Vec::with_capacity(16);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&CW_WIDTH_HEIGHT.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&u32::from(width).to_le_bytes());
    body.extend_from_slice(&u32::from(height).to_le_bytes());
    handle_configure_window(
        state,
        backend,
        None,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        &body,
    )
    .map(drop)
}

/// Ask a window's client to close it, as a window manager does: a
/// `WM_DELETE_WINDOW` `ClientMessage` when the window lists that protocol
/// in `WM_PROTOCOLS` (ICCCM 4.2.8.1), and `KillClient` on the window's
/// owner otherwise, which is what a window manager falls back to for a
/// client that cannot be asked.
///
/// A rootless server uses it when the display system that shows its
/// top-levels closes one: a Wayland compositor's `xdg_toplevel.close`, for
/// yserver's Wayland backend.
///
/// # Errors
///
/// As the requests' handlers.
pub fn close_window_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
) -> io::Result<()> {
    let delete = state.atoms.id_for("WM_DELETE_WINDOW");
    let asks = state
        .atoms
        .id_for("WM_PROTOCOLS")
        .zip(delete)
        .filter(|(protocols, delete)| {
            state
                .resources
                .window(window)
                .and_then(|w| w.properties.get(protocols))
                .is_some_and(|value| {
                    value
                        .data
                        .chunks_exact(4)
                        .any(|atom| atom == delete.0.to_le_bytes())
                })
        });
    if let Some((protocols, delete)) = asks {
        // SendEvent(propagate = false, destination = the window, no event
        // mask: to the window's creator), carrying a ClientMessage of
        // format 32: WM_PROTOCOLS, then WM_DELETE_WINDOW and CurrentTime.
        let mut body = Vec::with_capacity(40);
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        let mut event = [0u8; 32];
        event[0] = 33; // ClientMessage
        event[1] = 32; // format
        event[4..8].copy_from_slice(&window.0.to_le_bytes());
        event[8..12].copy_from_slice(&protocols.0.to_le_bytes());
        event[12..16].copy_from_slice(&delete.0.to_le_bytes());
        body.extend_from_slice(&event);
        let header = RequestHeader {
            opcode: 25,
            data: 0,
            length_units: 11,
        };
        return handle_send_event(
            state,
            crate::resources::SERVER_OWNER,
            SequenceNumber(0),
            header,
            &body,
        )
        .map(drop);
    }
    handle_kill_client(
        state,
        backend,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        &window.0.to_le_bytes(),
    )
    .map(drop)
}
