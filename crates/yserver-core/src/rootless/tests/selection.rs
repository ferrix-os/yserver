//! Extension X4: the host's clipboard bridge (`rootless/selection.rs`).

use yserver_protocol::x11::{AtomId, ResourceId};

use super::RecordingHost;
use crate::{
    resources::{ROOT_WINDOW, ROOTLESS_SELECTION_WINDOW},
    rootless::{SelectionEvent, selection},
};

const WINDOW: u32 = 0x0020_0001;

fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A client's ConvertSelection of a selection the server's window owns is
/// the host's to answer: it is queued, and nothing reaches the client.
#[test]
fn a_selection_the_server_owns_is_asked_of_the_host() {
    let mut host = RecordingHost::new();
    host.create_window(1, WINDOW, ROOT_WINDOW.0, (0, 0), (10, 10), 0, false);
    let clipboard = AtomId(host.atom("CLIPBOARD"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("XSEL_DATA"));
    selection::set_selection_owner(&mut host.state, clipboard, true).expect("own");
    // ConvertSelection (24): requestor, selection, target, property, time.
    let reply = host.request(
        1,
        24,
        0,
        &words(&[WINDOW, clipboard.0, utf8.0, property.0, 0]),
    );
    assert!(reply.is_empty(), "the client waits for the host: {reply:?}");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Request {
            time: 0,
            requestor: ResourceId(WINDOW),
            selection: clipboard,
            target: utf8,
            property,
        }]
    );
}

/// The server's ConvertSelection of a selection nothing owns is refused to
/// the host; an owner's SelectionNotify for the server's window is the
/// host's.
#[test]
fn the_server_hears_the_answer_to_its_own_convert_selection() {
    let mut host = RecordingHost::new();
    let primary = AtomId(host.atom("PRIMARY"));
    let utf8 = AtomId(host.atom("UTF8_STRING"));
    let property = AtomId(host.atom("_YSERVER_SELECTION"));
    selection::convert_selection(&mut host.state, primary, utf8, property).expect("convert");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Notify {
            selection: primary,
            target: utf8,
            property: AtomId(0),
        }]
    );
    // SendEvent (25) of a SelectionNotify to the server's window.
    let mut event = [0u8; 32];
    event[0] = 31;
    event[8..12].copy_from_slice(&ROOTLESS_SELECTION_WINDOW.0.to_le_bytes());
    event[12..16].copy_from_slice(&primary.0.to_le_bytes());
    event[16..20].copy_from_slice(&utf8.0.to_le_bytes());
    event[20..24].copy_from_slice(&property.0.to_le_bytes());
    let mut body = words(&[ROOTLESS_SELECTION_WINDOW.0, 0]);
    body.extend_from_slice(&event);
    let reply = host.request(1, 25, 0, &body);
    assert!(reply.is_empty(), "no error: {reply:?}");
    assert_eq!(
        selection::take_selection_events(&mut host.state),
        [SelectionEvent::Notify {
            selection: primary,
            target: utf8,
            property,
        }]
    );
}
