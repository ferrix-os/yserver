//! The topology against a host that records what it is asked.

use std::io;

use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

use super::*;
use crate::{
    backend::WindowHandle,
    properties::{PropertyFormat, PropertyValue},
    rootless::Damage,
};

/// What the host was asked, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Toplevel(ToplevelSpec<u32>),
    Destroy(u32),
    Title(u32, String),
    Class(u32, String),
    Parent(u32, Option<u32>),
    Limits(u32, SizeLimits),
    Draw(u32, (u32, u32), Vec<Damage>),
    Frame(u32),
}

#[derive(Default)]
struct Host {
    next: u32,
    calls: Vec<Call>,
    /// Surfaces the host has configured.
    ready: Vec<u32>,
}

impl Host {
    fn take(&mut self) -> Vec<Call> {
        std::mem::take(&mut self.calls)
    }

    fn make(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
}

impl HostWindows for Host {
    type Surface = u32;

    fn create_toplevel(&mut self, spec: &ToplevelSpec<u32>) -> io::Result<u32> {
        self.calls.push(Call::Toplevel(spec.clone()));
        Ok(self.make())
    }

    fn destroy(&mut self, surface: u32) {
        self.calls.push(Call::Destroy(surface));
    }

    fn set_title(&mut self, surface: u32, title: &str) {
        self.calls.push(Call::Title(surface, title.to_owned()));
    }

    fn set_class(&mut self, surface: u32, class: &str) {
        self.calls.push(Call::Class(surface, class.to_owned()));
    }

    fn set_parent(&mut self, surface: u32, parent: Option<u32>) {
        self.calls.push(Call::Parent(surface, parent));
    }

    fn set_size_limits(&mut self, surface: u32, limits: SizeLimits) {
        self.calls.push(Call::Limits(surface, limits));
    }

    fn ready(&self, surface: u32) -> bool {
        self.ready.contains(&surface)
    }

    fn draw(
        &mut self,
        surface: u32,
        size: (u32, u32),
        _opaque: bool,
        fill: &mut dyn FnMut(&mut [u8]) -> Option<Vec<Damage>>,
    ) -> io::Result<bool> {
        let mut buffer = vec![0; size.0 as usize * size.1 as usize * 4];
        let Some(damage) = fill(&mut buffer) else {
            return Ok(false);
        };
        self.calls.push(Call::Draw(surface, size, damage));
        Ok(true)
    }

    fn request_frame(&mut self, surface: u32) {
        self.calls.push(Call::Frame(surface));
    }
}

/// Every window's image is one colour, `version` its version: a draw of
/// the same colour moves the version without changing a pixel.
struct Images {
    version: u64,
    colour: u8,
    /// Rows `[first, last]` drawn in another colour.
    stripe: Option<(usize, usize)>,
}

impl RootlessRenderer for Images {
    fn image_version(&self, _host_xid: u32) -> Option<u64> {
        Some(self.version)
    }

    fn read_image_into(
        &mut self,
        _host_xid: u32,
        width: u16,
        _height: u16,
        border: u16,
        out: &mut [u8],
    ) -> bool {
        let stride = (usize::from(width) + 2 * usize::from(border)) * 4;
        for (y, row) in out.chunks_exact_mut(stride).enumerate() {
            let striped = self
                .stripe
                .is_some_and(|(first, last)| (first..=last).contains(&y));
            row.fill(if striped { !self.colour } else { self.colour });
        }
        true
    }
}

fn sync(
    topology: &mut Topology<u32>,
    state: &ServerState,
    host: &mut Host,
    images: &mut Images,
) -> Vec<HostRequest> {
    topology.sync(state, host, images)
}

const CLIENT: u32 = 3;

/// A mapped top-level child of the root, `width` x `height`, with host
/// XID `host`.
fn top_level(state: &mut ServerState, id: u32, host: u32, size: (u16, u16)) -> ResourceId {
    let window = ResourceId(id);
    state.resources.create_window(
        ClientId(CLIENT),
        CreateWindowRequest {
            depth: 24,
            window,
            parent: ROOT_WINDOW,
            x: 10,
            y: 20,
            width: size.0,
            height: size.1,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    if let Some(w) = state.resources.window_mut(window) {
        w.host_xid = Some(WindowHandle::from_raw_for_test(host));
    }
    let _ = state.resources.map_window(window);
    window
}

fn set_property(
    state: &mut ServerState,
    window: ResourceId,
    name: &str,
    format: PropertyFormat,
    data: &[u8],
) {
    let atom = state.atoms.intern(name, false);
    let r#type = state.atoms.intern("STRING", false);
    if let Some(w) = state.resources.window_mut(window) {
        let _ = w.properties.insert(
            atom,
            PropertyValue {
                r#type,
                format,
                data: data.to_vec(),
            },
        );
    }
}

fn images() -> Images {
    Images {
        version: 1,
        colour: 0x40,
        stripe: None,
    }
}

/// A mapped top-level becomes a host window with its title and class, is
/// drawn whole once the host has configured it, then only for a new
/// image, and goes when it unmaps.
#[test]
fn a_top_level_is_a_host_window_from_map_to_unmap() {
    let mut state = ServerState::new();
    let mut host = Host::default();
    let mut images = images();
    let mut topology = Topology::new();
    let window = top_level(&mut state, 0x0030_0001, 0x40, (64, 32));
    set_property(
        &mut state,
        window,
        "WM_NAME",
        PropertyFormat::F8,
        b"Event Tester",
    );
    set_property(
        &mut state,
        window,
        "WM_CLASS",
        PropertyFormat::F8,
        b"xev\0Xev\0",
    );

    assert!(sync(&mut topology, &state, &mut host, &mut images).is_empty());
    assert_eq!(
        host.take(),
        [Call::Toplevel(ToplevelSpec {
            title: "Event Tester".to_owned(),
            class: "Xev".to_owned(),
            size: (64, 32),
            parent: None,
            limits: SizeLimits::default(),
        })],
        "made, and not drawn before the host has given it a size"
    );
    assert_eq!(topology.host_xid_of(1), Some(0x40));
    assert_eq!(topology.window_of(&state, 1), Some(window));

    host.ready.push(1);
    topology.configured(1, 64, 32);
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(
        host.take(),
        [Call::Draw(1, (64, 32), vec![]), Call::Frame(1)],
        "drawn whole once configured"
    );

    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(host.take(), [], "nothing until the host shows the frame");
    topology.frame_done(1);
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(host.take(), [], "the same image is not drawn again");

    images.version = 2;
    images.stripe = Some((3, 5));
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(
        host.take(),
        [Call::Draw(1, (64, 32), vec![(0, 3, 64, 3)]), Call::Frame(1)],
        "a new image is damaged by the rows that changed"
    );

    let _ = state.resources.unmap_window(window);
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(host.take(), [Call::Destroy(1)]);
    assert_eq!(topology.host_xid_of(1), None);
}

/// A title set after the window is made follows it, and the class too; a
/// transient becomes a dialog of its parent's host window, made after it;
/// size hints become the host's size limits.
#[test]
fn names_parents_and_size_hints_follow_the_properties() {
    let mut state = ServerState::new();
    let mut host = Host::default();
    let mut images = images();
    let mut topology = Topology::new();
    let dialog = top_level(&mut state, 0x0030_0002, 0x42, (30, 30));
    let main = top_level(&mut state, 0x0030_0001, 0x41, (64, 32));
    set_property(
        &mut state,
        dialog,
        "WM_TRANSIENT_FOR",
        PropertyFormat::F32,
        &main.0.to_le_bytes(),
    );
    let mut hints = [0u32; 18];
    hints[0] = 0x30;
    (hints[5], hints[6], hints[7], hints[8]) = (30, 30, 30, 30);
    let hints: Vec<u8> = hints.iter().flat_map(|word| word.to_le_bytes()).collect();
    set_property(
        &mut state,
        dialog,
        "WM_NORMAL_HINTS",
        PropertyFormat::F32,
        &hints,
    );

    sync(&mut topology, &state, &mut host, &mut images);
    let calls = host.take();
    let fixed = SizeLimits {
        min: (30, 30),
        max: (30, 30),
    };
    assert!(
        matches!(&calls[..], [Call::Toplevel(first), Call::Toplevel(second)]
            if first.size == (64, 32) && second.parent == Some(1) && second.limits == fixed),
        "the dialog after its parent, made its dialog with its limits: {calls:?}"
    );

    set_property(&mut state, main, "WM_NAME", PropertyFormat::F8, b"renamed");
    let wm_name = state.atoms.intern("WM_NAME", false);
    topology.property_changed(&state, 0x41, wm_name);
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(host.take(), [Call::Title(1, "renamed".to_owned())]);

    set_property(
        &mut state,
        dialog,
        "WM_TRANSIENT_FOR",
        PropertyFormat::F32,
        &0u32.to_le_bytes(),
    );
    sync(&mut topology, &state, &mut host, &mut images);
    assert_eq!(host.take(), [Call::Parent(2, None)], "no longer a dialog");
}

/// What the host asks -- a size the window does not have, a close -- comes
/// back for the server to carry out, once.
#[test]
fn the_hosts_size_and_close_come_back_as_requests() {
    let mut state = ServerState::new();
    let mut host = Host::default();
    let mut images = images();
    let mut topology = Topology::new();
    let window = top_level(&mut state, 0x0030_0001, 0x40, (64, 32));
    sync(&mut topology, &state, &mut host, &mut images);

    topology.configured(1, 64, 32);
    assert!(
        sync(&mut topology, &state, &mut host, &mut images).is_empty(),
        "the size it has"
    );
    topology.configured(1, 800, 600);
    topology.close_requested(1);
    assert_eq!(
        sync(&mut topology, &state, &mut host, &mut images),
        [
            HostRequest::Resize {
                window,
                width: 800,
                height: 600
            },
            HostRequest::Close { window }
        ]
    );
    assert!(sync(&mut topology, &state, &mut host, &mut images).is_empty());
}

#[test]
fn the_class_is_the_second_string_or_the_only_one() {
    assert_eq!(class(b"xmessage\0Xmessage\0"), "Xmessage");
    assert_eq!(class(b"Xev"), "Xev");
    assert_eq!(class(b"xev\0"), "xev");
    assert_eq!(class(b""), "");
}

/// A `WM_SIZE_HINTS` value of 18 words with `flags`, the least size and
/// the greatest.
fn hints(flags: u32, min: (u32, u32), max: (u32, u32)) -> Vec<u8> {
    let mut words = [0_u32; 18];
    words[0] = flags;
    (words[5], words[6], words[7], words[8]) = (min.0, min.1, max.0, max.1);
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

#[test]
fn size_hints_give_limits_only_where_their_flags_say() {
    let fixed = size_limits(&hints(0x30, (640, 480), (640, 480)));
    assert_eq!(fixed.min, (640, 480));
    assert_eq!(fixed.max, (640, 480));
    let least = size_limits(&hints(0x10, (200, 100), (900, 900)));
    assert_eq!(
        (least.min, least.max),
        ((200, 100), (0, 0)),
        "PMaxSize unset"
    );
    assert_eq!(
        size_limits(&hints(0x0c, (1, 1), (2, 2))),
        SizeLimits::default(),
        "USSize and PPosition alone"
    );
    let negative = size_limits(&hints(0x30, (u32::MAX, 10), (640, 480)));
    assert_eq!(negative.min, (0, 10), "a negative size is no limit");
    assert_eq!(
        size_limits(&[0x30, 0, 0, 0]),
        SizeLimits::default(),
        "cut short"
    );
    assert_eq!(size_limits(&[]), SizeLimits::default());
}

#[test]
fn each_ewmh_direction_is_an_edge_or_a_move() {
    let edges: Vec<Option<u32>> = (0..12)
        .map(|direction| {
            move_resize_edges(ServerWmRequest::MoveResize {
                window: ResourceId(1),
                direction,
                button: 1,
            })
        })
        .collect();
    assert_eq!(
        edges,
        [
            Some(5),
            Some(1),
            Some(9),
            Some(8),
            Some(10),
            Some(2),
            Some(6),
            Some(4),
            Some(0),
            None,
            None,
            None
        ]
    );
}

#[test]
fn latin1_stops_at_a_nul() {
    assert_eq!(latin1(b"xev\0Xev"), "xev");
    assert_eq!(latin1(&[0x63, 0x61, 0x66, 0xe9]), "caf\u{e9}");
}
