use super::*;

/// The server's own `RedirectSubwindows(root)`: installed once, owned by
/// the server, backing the root's existing children, and never taken
/// over from a client that redirected them first.
#[test]
fn redirect_subwindows_for_server_backs_existing_children_once() {
    use crate::server::{CompositeRedirectMode, RedirectRecord};

    const CLIENT_ID: u32 = 1;
    const WINDOW_XID: u32 = 0x0010_0020;
    const HOST_XID: u32 = 0x0040_0020;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new().with_redirect_activation();
    state.resources.create_window(
        ClientId(CLIENT_ID),
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
        w.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));
    }
    // A backing is only allocated for a viewable window (Xorg
    // compCheckRedirect); an unmapped one gets it as it maps.
    assert!(
        state
            .resources
            .map_window(ResourceId(WINDOW_XID))
            .mapping_changed
    );

    let root = crate::resources::ROOT_WINDOW;
    assert!(redirect_subwindows_for_server(
        &mut state,
        &mut backend,
        root,
        CompositeRedirectMode::Manual
    ));
    let records = state.composite_redirects.subwindows_records(root);
    assert!(
        records
            .iter()
            .any(|r| r.owner == crate::resources::SERVER_OWNER)
    );
    let backing = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing.as_ref().map(|b| b.host_pixmap));
    assert!(
        backing.is_some(),
        "an existing viewable child gets its backing at once"
    );

    // Again: nothing new is allocated.
    assert!(redirect_subwindows_for_server(
        &mut state,
        &mut backend,
        root,
        CompositeRedirectMode::Manual
    ));
    let again = state
        .resources
        .window(ResourceId(WINDOW_XID))
        .and_then(|w| w.redirected_backing.as_ref().map(|b| b.host_pixmap));
    assert_eq!(backing, again);

    // A client's record is left alone.
    let children = state.resources.children(root).to_vec();
    assert!(state.composite_redirects.unredirect_subwindows(
        root,
        &children,
        crate::resources::SERVER_OWNER,
        CompositeRedirectMode::Manual
    ));
    state
        .composite_redirects
        .redirect_subwindows(
            root,
            &children,
            RedirectRecord {
                mode: CompositeRedirectMode::Automatic,
                owner: ClientId(CLIENT_ID),
            },
        )
        .unwrap();
    assert!(!redirect_subwindows_for_server(
        &mut state,
        &mut backend,
        root,
        CompositeRedirectMode::Manual
    ));
    let kept = state.composite_redirects.subwindows_records(root);
    assert!(kept.len() == 1 && kept[0].owner == ClientId(CLIENT_ID));
}

/// The server's own focus (a rootless backend's keyboard enter and
/// leave) moves the focus as SetInputFocus does, with the events, and
/// refuses a window that is not viewable.
#[test]
fn the_server_gives_the_focus_to_a_window_and_takes_it_away() {
    use yserver_protocol::x11::CreateWindowRequest;

    const CLIENT: u32 = 72;
    const WIN: u32 = 0x0072_0001;
    const HIDDEN: u32 = 0x0072_0002;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT);
    for win in [WIN, HIDDEN] {
        state.resources.create_window(
            ClientId(CLIENT),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(win),
                parent: ROOT_WINDOW,
                width: 200,
                height: 100,
                class: 1,
                visual: crate::resources::ROOT_VISUAL,
                ..Default::default()
            },
        );
    }
    assert!(state.resources.map_window(ResourceId(WIN)).mapping_changed);
    state
        .clients
        .get_mut(&CLIENT)
        .unwrap()
        .event_masks
        .insert(ResourceId(WIN), FOCUS_CHANGE_MASK);

    assert!(!set_input_focus_for_server(
        &mut state,
        Some(ResourceId(HIDDEN))
    ));
    assert_eq!(state.core_focus.raw, 1, "still PointerRoot");

    assert!(set_input_focus_for_server(
        &mut state,
        Some(ResourceId(WIN))
    ));
    assert_eq!(state.core_focus.raw, WIN);
    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.chunks_exact(32).any(|event| event[0] & 0x7f == 9),
        "FocusIn on the window"
    );

    assert!(set_input_focus_for_server(&mut state, None));
    assert_eq!(state.core_focus.raw, 0, "None");
    let bytes = read_all_available(&mut peer);
    assert!(
        bytes.chunks_exact(32).any(|event| event[0] & 0x7f == 10),
        "FocusOut from the window"
    );
}

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

    configure_window_for_server(&mut state, &mut backend, window, 640, 480).expect("configure");
    let size = state.resources.window(window).map(|w| (w.width, w.height));
    assert_eq!(size, Some((640, 480)));
    let out = read_all_available(&mut peer);
    assert!(
        out.chunks(32).any(|event| event[0] & 0x7f == 22),
        "the client is sent a ConfigureNotify"
    );

    // Zero, which X refuses, changes nothing.
    configure_window_for_server(&mut state, &mut backend, window, 0, 480).expect("configure");
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

    close_window_for_server(&mut state, &mut backend, window).expect("close");
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

/// The keyboard reaching a window that speaks `WM_TAKE_FOCUS` offers it
/// the focus with the time of the focus change; one that does not is
/// sent nothing.
#[test]
fn take_focus_for_server_offers_wm_take_focus() {
    use crate::properties::{PropertyFormat, PropertyValue};

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let window = server_helper_window(&mut state);
    assert!(
        !take_focus_for_server(&mut state, window),
        "no WM_PROTOCOLS, nothing to offer"
    );
    let protocols = state.atoms.intern("WM_PROTOCOLS", false);
    let take_focus = state.atoms.intern("WM_TAKE_FOCUS", false);
    if let Some(w) = state.resources.window_mut(window) {
        w.properties.insert(
            protocols,
            PropertyValue {
                r#type: AtomId(4), // ATOM
                format: PropertyFormat::F32,
                data: take_focus.0.to_le_bytes().to_vec(),
            },
        );
    }
    state.core_focus.time = 4242;
    let _ = read_all_available(&mut peer);

    assert!(take_focus_for_server(&mut state, window));
    let out = read_all_available(&mut peer);
    let message = out
        .chunks(32)
        .find(|event| event[0] & 0x7f == 33)
        .expect("a ClientMessage");
    assert_eq!(&message[4..8], &window.0.to_le_bytes());
    assert_eq!(&message[8..12], &protocols.0.to_le_bytes());
    assert_eq!(&message[12..16], &take_focus.0.to_le_bytes());
    assert_eq!(&message[16..20], &4242u32.to_le_bytes(), "the focus time");
}

/// Closing a window whose client cannot be asked disconnects it.
#[test]
fn close_window_for_server_kills_a_client_that_cannot_be_asked() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = server_helper_window(&mut state);

    close_window_for_server(&mut state, &mut backend, window).expect("close");
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

/// The server announces itself as the window manager the EWMH way: the
/// root and a window of its own name that window, which carries the
/// name; once there, it is not made again, and a window manager
/// client's announcement is left alone, while a stale one is replaced.
#[test]
fn the_server_announces_itself_as_the_window_manager() {
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let word = |state: &ServerState, window: ResourceId, name: &str| {
        let atom = state.atoms.id_for(name).expect("interned");
        let data = &state.resources.window(window).expect("window").properties[&atom].data;
        u32::from_le_bytes(data[0..4].try_into().expect("a word"))
    };

    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &["_NET_WM_NAME"])
        .expect("announce");
    let wm = ResourceId(word(&state, ROOT_WINDOW, "_NET_SUPPORTING_WM_CHECK"));
    assert_eq!(word(&state, wm, "_NET_SUPPORTING_WM_CHECK"), wm.0);
    assert_eq!(
        state.resources.window_owner(wm),
        Some(crate::resources::SERVER_OWNER)
    );
    let window = state.resources.window(wm).expect("window");
    assert_eq!(window.class, crate::resources::WindowClass::InputOnly);
    assert_eq!(window.map_state, MapState::Unmapped);
    let name = state.atoms.id_for("_NET_WM_NAME").expect("interned");
    assert_eq!(window.properties[&name].data, b"hyprix");
    assert_eq!(
        word(&state, ROOT_WINDOW, "_NET_SUPPORTED"),
        name.0,
        "the root lists the hints honoured"
    );

    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &[]).expect("again");
    assert_eq!(
        word(&state, ROOT_WINDOW, "_NET_SUPPORTING_WM_CHECK"),
        wm.0,
        "made once"
    );

    // A window manager client's own window is left alone.
    let client = server_helper_window(&mut state);
    let check = state
        .atoms
        .id_for("_NET_SUPPORTING_WM_CHECK")
        .expect("interned");
    let window_type = state.atoms.intern("WINDOW", false);
    change_property_for_server(
        &mut state,
        &mut backend,
        ROOT_WINDOW,
        check,
        window_type,
        32,
        &client.0.to_le_bytes(),
    )
    .expect("a client's announcement");
    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &[])
        .expect("left alone");
    assert_eq!(
        word(&state, ROOT_WINDOW, "_NET_SUPPORTING_WM_CHECK"),
        client.0
    );

    // One naming a window that is gone is the server's to replace.
    change_property_for_server(
        &mut state,
        &mut backend,
        ROOT_WINDOW,
        check,
        window_type,
        32,
        &0x0bad_0000u32.to_le_bytes(),
    )
    .expect("a stale announcement");
    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &[]).expect("replaced");
    let replaced = ResourceId(word(&state, ROOT_WINDOW, "_NET_SUPPORTING_WM_CHECK"));
    assert_eq!(
        word(&state, replaced, "_NET_SUPPORTING_WM_CHECK"),
        replaced.0
    );
}

/// A `_NET_WM_MOVERESIZE` a client sends the root is queued for the
/// backend only while the server is the window manager and lists it.
#[test]
fn a_move_resize_is_the_servers_while_it_manages_and_lists_it() {
    const CLIENT: u32 = 1;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    let _peer = install_client(&mut state, CLIENT);
    let move_resize = state.atoms.intern("_NET_WM_MOVERESIZE", false);
    let send = |state: &mut ServerState| {
        let mut body = Vec::with_capacity(40);
        body.extend_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        // SubstructureRedirect | SubstructureNotify, as EWMH says.
        body.extend_from_slice(&0x0018_0000u32.to_le_bytes());
        let mut event = [0u8; 32];
        event[0] = 33; // ClientMessage
        event[1] = 32;
        event[4..8].copy_from_slice(&0x0020_0001u32.to_le_bytes());
        event[8..12].copy_from_slice(&move_resize.0.to_le_bytes());
        event[12..16].copy_from_slice(&300u32.to_le_bytes()); // x_root
        event[16..20].copy_from_slice(&200u32.to_le_bytes()); // y_root
        event[20..24].copy_from_slice(&8u32.to_le_bytes()); // move
        event[24..28].copy_from_slice(&1u32.to_le_bytes()); // button 1
        body.extend_from_slice(&event);
        let header = yserver_protocol::x11::RequestHeader {
            opcode: 25,
            data: 0,
            length_units: 11,
        };
        handle_send_event(state, ClientId(CLIENT), SequenceNumber(1), header, &body)
            .expect("SendEvent");
    };

    send(&mut state);
    assert_eq!(state.server_wm_requests, [], "no window manager yet");

    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &["_NET_WM_NAME"])
        .expect("announce");
    send(&mut state);
    assert_eq!(state.server_wm_requests, [], "not listed");

    let net_supported = state.atoms.intern("_NET_SUPPORTED", false);
    let atom_type = state.atoms.intern("ATOM", false);
    change_property_for_server(
        &mut state,
        &mut backend,
        ROOT_WINDOW,
        net_supported,
        atom_type,
        32,
        &move_resize.0.to_le_bytes(),
    )
    .expect("listed");
    send(&mut state);
    assert_eq!(
        state.server_wm_requests,
        [crate::server::ServerWmRequest::MoveResize {
            window: ResourceId(0x0020_0001),
            direction: 8,
            button: 1,
        }]
    );
}

/// While the server stands in for the window manager, a top-level its
/// client maps is given `WM_STATE` `NormalState` and one it unmaps
/// `WithdrawnState`, each told to the client in a `PropertyNotify`
/// carrying the serial of the request that caused it, which Wine's X
/// driver compares with its own. With no window manager announced, the
/// server sets nothing, as there would be none to set it.
#[test]
fn a_top_level_is_given_wm_state_while_the_server_manages() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let window = server_helper_window(&mut state);
    state
        .clients
        .get_mut(&1)
        .expect("client 1")
        .event_masks
        .insert(window, 0x0040_0000); // PropertyChange
    let map = |state: &mut ServerState, backend: &mut RecordingBackend, serial: u16| {
        if let Some(client) = state.clients.get(&1) {
            client
                .last_sequence
                .store(serial, std::sync::atomic::Ordering::Relaxed);
        }
        handle_map_window(
            state,
            backend,
            None,
            ClientId(1),
            SequenceNumber(serial),
            &window.0.to_le_bytes(),
        )
        .expect("MapWindow");
    };
    let unmap = |state: &mut ServerState, backend: &mut RecordingBackend, serial: u16| {
        if let Some(client) = state.clients.get(&1) {
            client
                .last_sequence
                .store(serial, std::sync::atomic::Ordering::Relaxed);
        }
        handle_unmap_window(
            state,
            backend,
            None,
            ClientId(1),
            SequenceNumber(serial),
            &window.0.to_le_bytes(),
        )
        .expect("UnmapWindow");
    };
    let wm_state = |state: &ServerState| {
        state
            .atoms
            .id_for("WM_STATE")
            .and_then(|atom| state.resources.window_property(window, atom))
            .map(|value| value.data.clone())
    };
    // PropertyNotify (28) on the window: its serial, and the atom.
    let notified = |out: &[u8]| -> Vec<(u16, u32)> {
        out.chunks(32)
            .filter(|event| event[0] & 0x7f == 28)
            .map(|event| {
                (
                    u16::from_le_bytes([event[2], event[3]]),
                    u32::from_le_bytes(event[8..12].try_into().expect("atom")),
                )
            })
            .collect()
    };

    map(&mut state, &mut backend, 3);
    assert_eq!(wm_state(&state), None, "no window manager, no WM_STATE");
    unmap(&mut state, &mut backend, 4);

    announce_window_manager_for_server(&mut state, &mut backend, "hyprix", &[]).expect("announce");
    let atom = state.atoms.intern("WM_STATE", false);
    let _ = read_all_available(&mut peer);
    map(&mut state, &mut backend, 7);
    assert_eq!(wm_state(&state), Some([1, 0, 0, 0, 0, 0, 0, 0].to_vec()));
    assert_eq!(notified(&read_all_available(&mut peer)), [(7, atom.0)]);
    let value = state
        .resources
        .window_property(window, atom)
        .expect("WM_STATE");
    assert_eq!(value.r#type, atom, "of type WM_STATE");

    unmap(&mut state, &mut backend, 9);
    assert_eq!(wm_state(&state), Some([0; 8].to_vec()));
    assert_eq!(notified(&read_all_available(&mut peer)), [(9, atom.0)]);
}
