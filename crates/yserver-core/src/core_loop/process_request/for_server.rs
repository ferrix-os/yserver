//! Requests the server makes for itself, as a window manager would make
//! them: what a rootless server needs when another display system shows its
//! top-level windows and decides their size and their closing (yserver's
//! Wayland backend).

use std::io;

use yserver_protocol::x11::{RequestHeader, ResourceId, SequenceNumber};

use crate::{backend::Backend, server::ServerState};

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
    super::windows::handle_configure_window(
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
        return super::selection::handle_send_event(
            state,
            crate::resources::SERVER_OWNER,
            SequenceNumber(0),
            header,
            &body,
        )
        .map(drop);
    }
    super::misc::handle_kill_client(
        state,
        backend,
        crate::resources::SERVER_OWNER,
        SequenceNumber(0),
        &window.0.to_le_bytes(),
    )
    .map(drop)
}
