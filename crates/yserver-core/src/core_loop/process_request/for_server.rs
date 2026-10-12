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
    if let Some((protocols, delete)) = wm_protocol(state, window, "WM_DELETE_WINDOW") {
        // CurrentTime: ICCCM 4.2.8.1 asks for none in particular.
        return send_wm_protocol(state, window, protocols, delete, 0);
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

/// Offer a top-level the keyboard, as a window manager does once it has
/// given it the input focus: a `WM_TAKE_FOCUS` `ClientMessage` (ICCCM
/// 4.1.7) when the window lists that protocol in `WM_PROTOCOLS`, carrying
/// the time of the last focus change, so that a `SetInputFocus` the client
/// makes in answer is not older than it. Whether one was sent.
///
/// A client of the "globally active" input model -- `WM_TAKE_FOCUS` and
/// `WM_HINTS` input false, as Wine's X11 driver sets on every window -- takes
/// the focus only when it is offered so: Wine activates a window on
/// `WM_TAKE_FOCUS` and ignores a plain `FocusIn`, so a window the keyboard
/// reached without a click would show the caret of nothing and drop every
/// key. A rootless server uses this beside `set_input_focus_for_server`
/// when the display system that shows its top-levels gives one the
/// keyboard: a Wayland compositor's `wl_keyboard.enter`, for yserver's
/// Wayland backend.
pub fn take_focus_for_server(state: &mut ServerState, window: ResourceId) -> bool {
    let Some((protocols, take_focus)) = wm_protocol(state, window, "WM_TAKE_FOCUS") else {
        return false;
    };
    let time = state.core_focus.time;
    if let Err(error) = send_wm_protocol(state, window, protocols, take_focus, time) {
        debug!("WM_TAKE_FOCUS to 0x{:x}: {error}", window.0);
        return false;
    }
    true
}

/// `WM_PROTOCOLS` and the atom `name`, when `window` lists that protocol.
fn wm_protocol(state: &ServerState, window: ResourceId, name: &str) -> Option<(AtomId, AtomId)> {
    let protocols = state.atoms.id_for("WM_PROTOCOLS")?;
    let protocol = state.atoms.id_for(name)?;
    state
        .resources
        .window(window)
        .and_then(|w| w.properties.get(&protocols))
        .is_some_and(|value| {
            value
                .data
                .chunks_exact(4)
                .any(|atom| atom == protocol.0.to_le_bytes())
        })
        .then_some((protocols, protocol))
}

/// Send `window`'s client the `WM_PROTOCOLS` message `protocol` with
/// `time`, as a window manager does: SendEvent(propagate = false,
/// destination = the window, no event mask: to the window's creator),
/// carrying a `ClientMessage` of format 32: `WM_PROTOCOLS`, then the
/// protocol and the time.
fn send_wm_protocol(
    state: &mut ServerState,
    window: ResourceId,
    protocols: AtomId,
    protocol: AtomId,
    time: u32,
) -> io::Result<()> {
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let mut event = [0u8; 32];
    event[0] = 33; // ClientMessage
    event[1] = 32; // format
    event[4..8].copy_from_slice(&window.0.to_le_bytes());
    event[8..12].copy_from_slice(&protocols.0.to_le_bytes());
    event[12..16].copy_from_slice(&protocol.0.to_le_bytes());
    event[16..20].copy_from_slice(&time.to_le_bytes());
    body.extend_from_slice(&event);
    let header = RequestHeader {
        opcode: 25,
        data: 0,
        length_units: 11,
    };
    handle_send_event(
        state,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        header,
        &body,
    )
    .map(drop)
}

/// A window of the server's own that can own a selection and be sent one:
/// an unmapped `InputOnly` child of the root, owned by `SERVER_OWNER`, at
/// the first free id of the server's range. Made once; an existing one is
/// given back, and one a server reset took is made again.
///
/// A rootless server's bridge to another display system's clipboard
/// (yserver's Wayland backend) owns X selections with it on that system's
/// behalf, and asks X clients for theirs with it, as Xwayland does with a
/// window of its own. What the selection protocol sends it is queued in
/// [`ServerState::server_selection_events`].
pub fn selection_window_for_server(
    state: &mut ServerState,
    known: Option<ResourceId>,
) -> ResourceId {
    if let Some(window) = known
        && state.resources.window_owner(window) == Some(crate::resources::SERVER_OWNER)
    {
        return window;
    }
    let window = server_input_only_window(state);
    debug!("the server's selection window is 0x{:x}", window.0);
    window
}

/// A new unmapped `InputOnly` child of the root, owned by `SERVER_OWNER`,
/// at the first free id of the server's range: nothing to draw, nothing a
/// client's disconnect takes.
fn server_input_only_window(state: &mut ServerState) -> ResourceId {
    let mut id = 0x110;
    while state.xid_occupied(id) {
        id += 1;
    }
    let window = ResourceId(id);
    state.resources.create_window(
        crate::resources::SERVER_OWNER,
        x11::CreateWindowRequest {
            window,
            parent: ROOT_WINDOW,
            width: 1,
            height: 1,
            class: 2, // InputOnly
            ..x11::CreateWindowRequest::default()
        },
    );
    window
}

/// Say that the server itself manages the windows, under the name `name`,
/// as an EWMH window manager does (EWMH 1.5, `_NET_SUPPORTING_WM_CHECK`):
/// a window of the server's own (an unmapped `InputOnly` child of the root)
/// whose `_NET_SUPPORTING_WM_CHECK` names itself and whose `_NET_WM_NAME`
/// is `name`, the root's `_NET_SUPPORTING_WM_CHECK` naming that window, and
/// the root's `_NET_SUPPORTED` listing `supported`, the hints honoured.
///
/// A rootless server whose top-level windows another display system shows
/// (yserver's Wayland backend, where the compositor manages each) has no X
/// window manager, so without this clients find none: Steam reports the
/// window manager as `<Unknown>`. Xwayland's compositors say it with a
/// window of theirs the same way.
///
/// Cheap when the root names a window that exists, so it may be called on
/// every loop iteration, which also puts it back after a server reset. A
/// window manager client that announced itself first is left alone.
///
/// # Errors
///
/// As `ChangeProperty`'s handler.
pub fn announce_window_manager_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    name: &str,
    supported: &[&str],
) -> io::Result<()> {
    let check = state.atoms.intern("_NET_SUPPORTING_WM_CHECK", false);
    let announced = state
        .resources
        .window(ROOT_WINDOW)
        .and_then(|root| root.properties.get(&check))
        .and_then(|value| value.data.get(0..4))
        .and_then(|data| data.try_into().ok())
        .map(|data| ResourceId(u32::from_le_bytes(data)));
    if announced.is_some_and(|window| state.resources.window(window).is_some()) {
        return Ok(());
    }
    let window = server_input_only_window(state);
    let net_wm_name = state.atoms.intern("_NET_WM_NAME", false);
    let utf8 = state.atoms.intern("UTF8_STRING", false);
    let net_supported = state.atoms.intern("_NET_SUPPORTED", false);
    let window_type = state.atoms.intern("WINDOW", false);
    let atom_type = state.atoms.intern("ATOM", false);
    let supported: Vec<u8> = supported
        .iter()
        .flat_map(|hint| state.atoms.intern(hint, false).0.to_le_bytes())
        .collect();
    let id = window.0.to_le_bytes();
    change_property_for_server(state, backend, window, check, window_type, 32, &id)?;
    change_property_for_server(
        state,
        backend,
        window,
        net_wm_name,
        utf8,
        8,
        name.as_bytes(),
    )?;
    change_property_for_server(
        state,
        backend,
        ROOT_WINDOW,
        net_supported,
        atom_type,
        32,
        &supported,
    )?;
    change_property_for_server(state, backend, ROOT_WINDOW, check, window_type, 32, &id)?;
    debug!(
        "the server is the window manager {name:?}, window 0x{:x}",
        window.0
    );
    Ok(())
}

/// Whether the server itself is the window manager the root names
/// (`announce_window_manager_for_server`) and lists `hint` in the root's
/// `_NET_SUPPORTED`: whether a client's request of that kind is the
/// server's to carry out.
pub(super) fn server_manages(state: &ServerState, hint: AtomId) -> bool {
    let root_word_list = |name: &str| -> Vec<u32> {
        state
            .atoms
            .id_for(name)
            .and_then(|atom| state.resources.window(ROOT_WINDOW)?.properties.get(&atom))
            .map(|value| {
                value
                    .data
                    .chunks_exact(4)
                    .filter_map(|word| word.try_into().ok().map(u32::from_le_bytes))
                    .collect()
            })
            .unwrap_or_default()
    };
    server_is_window_manager(state) && root_word_list("_NET_SUPPORTED").contains(&hint.0)
}

/// Whether the window the root's `_NET_SUPPORTING_WM_CHECK` names is the
/// server's own (`announce_window_manager_for_server`): whether the server
/// stands in for the window manager.
fn server_is_window_manager(state: &ServerState) -> bool {
    state
        .atoms
        .id_for("_NET_SUPPORTING_WM_CHECK")
        .and_then(|atom| state.resources.window(ROOT_WINDOW)?.properties.get(&atom))
        .and_then(|value| value.data.get(0..4))
        .and_then(|data| <[u8; 4]>::try_from(data).ok())
        .is_some_and(|data| {
            state
                .resources
                .window_owner(ResourceId(u32::from_le_bytes(data)))
                == Some(crate::resources::SERVER_OWNER)
        })
}

/// ICCCM 4.1.3.1's `WithdrawnState` and `NormalState`, as `WM_STATE` says
/// them.
pub(super) const WM_STATE_WITHDRAWN: u32 = 0;
pub(super) const WM_STATE_NORMAL: u32 = 1;

/// Set `WM_STATE` on a top-level `window` its client just mapped
/// ([`WM_STATE_NORMAL`]) or unmapped ([`WM_STATE_WITHDRAWN`]), as a window
/// manager does (ICCCM 4.1.3.1 and 4.1.4), when the server stands in for
/// one ([`server_is_window_manager`]).
///
/// A client that finds a window manager may wait for `WM_STATE` to say its
/// window changed state. Wine's X driver does: until the `PropertyNotify`
/// of `WM_STATE` comes, with a serial no older than its `MapWindow`'s, it
/// holds every later change of the window's state and ignores every size
/// the window is given. With nothing setting it, Wine's notepad kept the
/// size it asked for and not the size the compositor gave its window.
/// Xwayland's compositors set it from their X window managers. Called
/// within the client's request, the notify carries that request's serial.
pub(super) fn set_wm_state_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    wm_state: u32,
) -> io::Result<()> {
    let top_level = window != ROOT_WINDOW
        && state
            .resources
            .window(window)
            .is_some_and(|w| w.parent == ROOT_WINDOW && !w.override_redirect);
    if !top_level || !server_is_window_manager(state) {
        return Ok(());
    }
    let property = state.atoms.intern("WM_STATE", false);
    // The state, then the icon window: none.
    let mut data = Vec::with_capacity(8);
    data.extend_from_slice(&wm_state.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    change_property_for_server(state, backend, window, property, property, 32, &data)
}

/// Make the server's `window` the owner of `selection`, or give it up with
/// `None`, as a client's `SetSelectionOwner` at `CurrentTime` would: the
/// previous owner hears `SelectionClear`, and XFIXES subscribers are told.
///
/// # Errors
///
/// As the request's handler.
pub fn set_selection_owner_for_server(
    state: &mut ServerState,
    selection: AtomId,
    window: Option<ResourceId>,
) -> io::Result<()> {
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&window.map_or(0, |window| window.0).to_le_bytes());
    body.extend_from_slice(&selection.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    handle_set_selection_owner(
        state,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        &body,
    )
    .map(drop)
}

/// Ask for `selection` as `target`, to be put in `property` on the server's
/// `requestor`, as a client's `ConvertSelection` at `CurrentTime` would.
/// The answer is a [`crate::server::ServerSelectionEvent::Notify`].
///
/// # Errors
///
/// As the request's handler.
pub fn convert_selection_for_server(
    state: &mut ServerState,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> io::Result<()> {
    let mut body = Vec::with_capacity(20);
    for word in [requestor.0, selection.0, target.0, property.0, 0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    handle_convert_selection(
        state,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        &body,
    )
    .map(drop)
}

/// Replace `property` on `window` with `data` of `type` in `format` (8, 16
/// or 32, `data` in this machine's order), as a client's `ChangeProperty`
/// would: the window's watchers hear `PropertyNotify`. How the server
/// answers a `SelectionRequest` it was sent.
///
/// # Errors
///
/// As the request's handler.
pub fn change_property_for_server(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    property: AtomId,
    r#type: AtomId,
    format: u8,
    data: &[u8],
) -> io::Result<()> {
    let unit = usize::from(format / 8).max(1);
    let length = u32::try_from(data.len() / unit).unwrap_or(u32::MAX);
    let mut body = Vec::with_capacity(20 + data.len() + 3);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&r#type.0.to_le_bytes());
    body.extend_from_slice(&[format, 0, 0, 0]);
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(data);
    while body.len() % 4 != 0 {
        body.push(0);
    }
    let header = RequestHeader {
        opcode: 18,
        data: 0, // Replace
        length_units: u32::try_from(body.len() / 4 + 1).unwrap_or(u32::MAX),
    };
    handle_change_property(
        state,
        backend,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        header,
        &body,
    )
    .map(drop)
}

/// Tell `requestor`'s client the answer to its `ConvertSelection` is in
/// `property` (`None` for a refusal), as a selection owner's `SendEvent`
/// of a `SelectionNotify` would.
///
/// # Errors
///
/// As the request's handler.
pub fn send_selection_notify_for_server(
    state: &mut ServerState,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> io::Result<()> {
    let mut body = Vec::with_capacity(40);
    body.extend_from_slice(&requestor.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let mut event = [0u8; 32];
    event[0] = 31; // SelectionNotify
    event[4..8].copy_from_slice(&time.to_le_bytes());
    event[8..12].copy_from_slice(&requestor.0.to_le_bytes());
    event[12..16].copy_from_slice(&selection.0.to_le_bytes());
    event[16..20].copy_from_slice(&target.0.to_le_bytes());
    event[20..24].copy_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&event);
    let header = RequestHeader {
        opcode: 25,
        data: 0,
        length_units: 11,
    };
    handle_send_event(
        state,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        header,
        &body,
    )
    .map(drop)
}
