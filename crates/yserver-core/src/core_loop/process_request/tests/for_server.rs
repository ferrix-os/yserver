//! The requests the server makes for itself (`for_server.rs`).

use super::*;

/// A root child of client 1, 100x50, with a host xid.
fn server_helper_window(state: &mut ServerState) -> ResourceId {
    const WINDOW_XID: u32 = 0x0010_0030;
    state.resources.create_window(
        ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            width: 100,
            height: 50,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    if let Some(w) = state.resources.window_mut(ResourceId(WINDOW_XID)) {
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0x0040_0030));
    }
    ResourceId(WINDOW_XID)
}

/// A compositor's size for a top-level reaches the window and its
/// client, as a window manager's `ConfigureWindow` would.
#[test]
fn configure_window_for_server_resizes_and_tells_the_client() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = server_helper_window(&mut state);
    state
        .clients
        .get_mut(&1)
        .expect("client 1")
        .event_masks
        .insert(window, 0x0002_0000); // StructureNotify
    let _ = read_all_available(&mut peer);

    crate::core_loop::process_request::configure_window_for_server(
        &mut state,
        &mut backend,
        window,
        640,
        480,
    )
    .expect("configure");
    let size = state.resources.window(window).map(|w| (w.width, w.height));
    assert_eq!(size, Some((640, 480)));
    let out = read_all_available(&mut peer);
    assert!(
        out.chunks(32).any(|event| event[0] & 0x7f == 22),
        "the client is sent a ConfigureNotify"
    );

    // Zero, which X refuses, changes nothing.
    crate::core_loop::process_request::configure_window_for_server(
        &mut state,
        &mut backend,
        window,
        0,
        480,
    )
    .expect("configure");
    let size = state.resources.window(window).map(|w| (w.width, w.height));
    assert_eq!(size, Some((640, 480)));
}

/// Closing a window that speaks `WM_DELETE_WINDOW` asks its client.
#[test]
fn close_window_for_server_sends_wm_delete_window() {
    use crate::properties::{PropertyFormat, PropertyValue};

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = server_helper_window(&mut state);
    let protocols = state.atoms.intern("WM_PROTOCOLS", false);
    let delete = state.atoms.intern("WM_DELETE_WINDOW", false);
    if let Some(w) = state.resources.window_mut(window) {
        w.properties.insert(
            protocols,
            PropertyValue {
                r#type: AtomId(4), // ATOM
                format: PropertyFormat::F32,
                data: delete.0.to_le_bytes().to_vec(),
            },
        );
    }
    let _ = read_all_available(&mut peer);

    crate::core_loop::process_request::close_window_for_server(&mut state, &mut backend, window)
        .expect("close");
    let out = read_all_available(&mut peer);
    let message = out
        .chunks(32)
        .find(|event| event[0] & 0x7f == 33)
        .expect("a ClientMessage");
    assert_eq!(&message[4..8], &window.0.to_le_bytes());
    assert_eq!(&message[8..12], &protocols.0.to_le_bytes());
    assert_eq!(&message[12..16], &delete.0.to_le_bytes());
    assert!(
        state.clients.contains_key(&1),
        "the client is asked, not killed"
    );
}

/// Closing a window whose client cannot be asked disconnects it.
#[test]
fn close_window_for_server_kills_a_client_that_cannot_be_asked() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = server_helper_window(&mut state);

    crate::core_loop::process_request::close_window_for_server(&mut state, &mut backend, window)
        .expect("close");
    assert!(!state.clients.contains_key(&1), "the client is gone");
}
