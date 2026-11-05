//! A host's clipboard bridge (extension X4): a window of the server's own
//! that owns X selections on the host's behalf and asks X clients for
//! theirs, as Xwayland does with a window of its own.
//!
//! A server window has no connection, so what the selection protocol would
//! send it -- the `SelectionRequest` a client's `ConvertSelection` sends a
//! selection's owner, and the `SelectionNotify` an owner sends the server
//! window that asked -- is queued in `Rootless` for the host to take
//! ([`take_selection_events`]). The server's own requests go through the
//! request handlers, so every client hears what it would from another
//! client's.

use std::io;

use yserver_protocol::x11::{AtomId, RequestHeader, ResourceId, SequenceNumber};

use crate::{
    backend::Backend,
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW, SERVER_OWNER},
    server::ServerState,
};

/// A selection event for the server's selection window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionEvent {
    /// A client's `ConvertSelection` of a selection the server's window
    /// owns: the `SelectionRequest` the owner would be sent.
    Request {
        /// The request's time, or 0 for `CurrentTime`.
        time: u32,
        /// The client's window the answer goes to.
        requestor: ResourceId,
        selection: AtomId,
        /// The type asked for.
        target: AtomId,
        /// Where on `requestor` to put it; `None` from an obsolete client,
        /// which means `target`.
        property: AtomId,
    },
    /// The `SelectionNotify` a selection's owner sent the server's window
    /// that asked for it, or the server sent itself when nothing owned it.
    Notify {
        selection: AtomId,
        /// The type asked for.
        target: AtomId,
        /// Where the answer is on the server's window; `None` when there is
        /// none.
        property: AtomId,
    },
}

/// The server's selection window, made the first time it is asked for (and
/// again after a server reset took it): an unmapped `InputOnly` child of the
/// root.
#[must_use]
pub fn selection_window(state: &mut ServerState) -> ResourceId {
    if state.resources.window(ROOTLESS_SELECTION_WINDOW).is_none() {
        super::wm::create_input_only_as_server(state, ROOTLESS_SELECTION_WINDOW);
    }
    ROOTLESS_SELECTION_WINDOW
}

/// What was sent the server's selection window since the last call.
pub fn take_selection_events(state: &mut ServerState) -> Vec<SelectionEvent> {
    state
        .rootless
        .as_mut()
        .map(|r| std::mem::take(&mut r.selection_events))
        .unwrap_or_default()
}

fn is_server_window(state: &ServerState, window: ResourceId) -> bool {
    window != ROOT_WINDOW && state.resources.window_owner(window) == Some(SERVER_OWNER)
}

/// `ConvertSelection` hook: a selection the server's window owns is
/// answered by the host. Whether the request was taken here.
pub(crate) fn note_request(
    state: &mut ServerState,
    owner: ResourceId,
    event: SelectionEvent,
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, owner) {
        return false;
    }
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection_events.push(event);
    }
    true
}

/// `ConvertSelection` hook, for a selection nothing owns: the server's
/// window that asked hears the refusal. Whether it was taken here.
pub(crate) fn note_refusal(
    state: &mut ServerState,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, requestor) {
        return false;
    }
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection_events.push(SelectionEvent::Notify {
            selection,
            target,
            property: AtomId(0),
        });
    }
    true
}

/// `SendEvent` hook: a `SelectionNotify` for the server's window, the
/// answer to a [`convert_selection`]. Whether it was taken here.
pub(crate) fn note_selection_notify(
    state: &mut ServerState,
    destination: ResourceId,
    words: [u32; 3],
) -> bool {
    if state.rootless.is_none() || !is_server_window(state, destination) {
        return false;
    }
    let [selection, target, property] = words;
    if let Some(rootless) = state.rootless.as_mut() {
        rootless.selection_events.push(SelectionEvent::Notify {
            selection: AtomId(selection),
            target: AtomId(target),
            property: AtomId(property),
        });
    }
    true
}

/// Make the server's selection window the owner of `selection`, or give it
/// up (`own == false`), as a client's `SetSelectionOwner` at `CurrentTime`
/// would: the previous owner hears `SelectionClear`, and XFIXES subscribers
/// are told.
///
/// # Errors
///
/// As the request's handler.
pub fn set_selection_owner(
    state: &mut ServerState,
    selection: AtomId,
    own: bool,
) -> io::Result<()> {
    let window = if own { selection_window(state).0 } else { 0 };
    let mut body = Vec::with_capacity(12);
    for word in [window, selection.0, 0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    crate::core_loop::process_request::handle_set_selection_owner(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        &body,
    )
    .map(drop)
}

/// Ask for `selection` as `target`, to be put in `property` on the server's
/// selection window, as a client's `ConvertSelection` at `CurrentTime`
/// would. The answer is a [`SelectionEvent::Notify`].
///
/// # Errors
///
/// As the request's handler.
pub fn convert_selection(
    state: &mut ServerState,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) -> io::Result<()> {
    let requestor = selection_window(state);
    let mut body = Vec::with_capacity(20);
    for word in [requestor.0, selection.0, target.0, property.0, 0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    crate::core_loop::process_request::handle_convert_selection(
        state,
        SERVER_OWNER,
        SequenceNumber(0),
        &body,
    )
    .map(drop)
}

/// Tell `requestor`'s client the answer to its `ConvertSelection` is in
/// `property` (`None` for a refusal), as a selection owner's `SendEvent` of a
/// `SelectionNotify` would.
///
/// # Errors
///
/// As the request's handler.
pub fn send_selection_notify(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    time: u32,
    requestor: ResourceId,
    [selection, target, property]: [AtomId; 3],
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
    crate::core_loop::process_request::handle_send_event(
        state,
        backend,
        SERVER_OWNER,
        SequenceNumber(0),
        header,
        &body,
    )
    .map(drop)
}

/// Replace `property` on `window` with `data` of `type` in `format` (8, 16
/// or 32 bits, `data` in this machine's order), as a client's
/// `ChangeProperty` would: the window's watchers hear `PropertyNotify`. How
/// the host answers a [`SelectionEvent::Request`]. A format other than 8,
/// 16 or 32 is ignored.
pub fn change_property(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    window: ResourceId,
    [property, r#type]: [AtomId; 2],
    format: u8,
    data: Vec<u8>,
) {
    use crate::properties::PropertyFormat;
    let format = match format {
        8 => PropertyFormat::F8,
        16 => PropertyFormat::F16,
        32 => PropertyFormat::F32,
        _ => return,
    };
    crate::core_loop::process_request::change_property_as_server(
        state, backend, window, property, r#type, format, data,
    );
}
