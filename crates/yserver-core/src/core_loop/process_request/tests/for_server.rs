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
