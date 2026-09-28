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
