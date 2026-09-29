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

/// A selection the server's own window owns is asked of the server,
/// and its answer reaches the client; a client's answer to the
/// server's own request is the server's to take.
#[test]
fn a_selection_the_server_owns_is_asked_of_and_answered_by_the_server() {
    use crate::server::ServerSelectionEvent;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let client_window = server_helper_window(&mut state);
    let clipboard = state.atoms.intern("CLIPBOARD", false);
    let utf8 = state.atoms.intern("UTF8_STRING", false);
    let property = state.atoms.intern("PASTE", false);

    let server = selection_window_for_server(&mut state, None);
    assert_eq!(
        selection_window_for_server(&mut state, Some(server)),
        server
    );
    assert_eq!(
        state.resources.window_owner(server),
        Some(crate::resources::SERVER_OWNER)
    );
    set_selection_owner_for_server(&mut state, clipboard, Some(server)).expect("own");
    assert_eq!(
        state.selections.get(&clipboard).map(|held| held.0),
        Some(server)
    );

    // The client's ConvertSelection is the server's to answer.
    let mut body = Vec::new();
    for word in [client_window.0, clipboard.0, utf8.0, property.0, 7] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    let _ = read_all_available(&mut peer);
    handle_convert_selection(&mut state, ClientId(1), SequenceNumber(1), &body).expect("convert");
    assert_eq!(
        std::mem::take(&mut state.server_selection_events),
        vec![ServerSelectionEvent::Request {
            time: 7,
            owner: server,
            requestor: client_window,
            selection: clipboard,
            target: utf8,
            property,
        }]
    );
    change_property_for_server(
        &mut state,
        &mut backend,
        client_window,
        property,
        utf8,
        8,
        b"hi",
    )
    .expect("put");
    assert_eq!(
        state
            .resources
            .window_property(client_window, property)
            .map(|value| value.data.clone()),
        Some(b"hi".to_vec())
    );
    send_selection_notify_for_server(&mut state, 7, client_window, clipboard, utf8, property)
        .expect("notify");
    let out = read_all_available(&mut peer);
    let notify = out
        .chunks(32)
        .find(|event| event[0] & 0x7f == 31)
        .expect("a SelectionNotify");
    assert_eq!(&notify[20..24], &property.0.to_le_bytes());

    // The client owns it now; the server asks, and the client's
    // SendEvent of the answer is queued for the server.
    let mut body = Vec::new();
    for word in [client_window.0, clipboard.0, 0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    handle_set_selection_owner(&mut state, ClientId(1), SequenceNumber(2), &body)
        .expect("the client owns it");
    convert_selection_for_server(&mut state, server, clipboard, utf8, property)
        .expect("the server asks");
    let out = read_all_available(&mut peer);
    assert!(
        out.chunks(32).any(|event| event[0] & 0x7f == 30),
        "the client is sent a SelectionRequest"
    );
    let mut body = Vec::new();
    body.extend_from_slice(&server.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let mut event = [0u8; 32];
    event[0] = 31;
    event[8..12].copy_from_slice(&server.0.to_le_bytes());
    event[12..16].copy_from_slice(&clipboard.0.to_le_bytes());
    event[16..20].copy_from_slice(&utf8.0.to_le_bytes());
    event[20..24].copy_from_slice(&property.0.to_le_bytes());
    body.extend_from_slice(&event);
    let header = RequestHeader {
        opcode: 25,
        data: 0,
        length_units: 11,
    };
    handle_send_event(&mut state, ClientId(1), SequenceNumber(3), header, &body).expect("send");
    assert_eq!(
        std::mem::take(&mut state.server_selection_events),
        vec![ServerSelectionEvent::Notify {
            requestor: server,
            selection: clipboard,
            target: utf8,
            property,
        }]
    );

    // With no owner, the server hears the refusal.
    state.selections.clear();
    convert_selection_for_server(&mut state, server, clipboard, utf8, property)
        .expect("the server asks");
    assert_eq!(
        state.server_selection_events,
        vec![ServerSelectionEvent::Notify {
            requestor: server,
            selection: clipboard,
            target: utf8,
            property: AtomId(0),
        }]
    );
}
