//! Direct frames on Wayland: a client's LINEAR DRI3 buffer made a
//! `zwp_linux_dmabuf_v1` buffer of the compositor's and attached as its
//! window's frame (`RootlessHost::submit_direct`), instead of a frame read
//! back into `wl_shm`.
//!
//! Nothing an X client controls may make the compositor end the connection,
//! which would end every window with it. The compositor may answer
//! `zwp_linux_buffer_params_v1.create` with a fatal protocol error for bad
//! arguments (`incomplete`, `invalid_format`, `invalid_dimensions`,
//! `out_of_bounds`), and with the `failed` event for a buffer it cannot use
//! otherwise; what keeps the arguments good is ours to check:
//!
//! * One plane, added once, then `create` once per params object; never
//!   `create_immed`, which may answer an import failure with a fatal error
//!   too.
//! * Only a format the compositor named with `DRM_FORMAT_MOD_LINEAR` in its
//!   `zwp_linux_dmabuf_v1.modifier` events (bound at version 3, where
//!   formats and modifiers are events) is sent: `XRGB8888` below depth 32,
//!   `ARGB8888` at it, as the `wl_shm` frames are. The host takes direct
//!   frames of a depth only once the compositor named its format.
//! * The size (non-zero), offset and stride were checked when the client's
//!   buffer was imported (`kms/render/backend/kms/direct_frames.rs`): one
//!   row of stride at least, and `offset + stride * height` within the
//!   dma-buf and within 32 bits.
//!
//! Until `created` comes, and after `failed`, a frame of that buffer is
//! declined and its window read back. The compositor's buffer is destroyed
//! when the platform says the client buffer went (`forget_direct`); one
//! asked for and not answered yet then is destroyed when the answer comes.
//! A `wl_buffer.release` of one is `FrameEvent::DirectReleased`.

use std::{
    collections::{HashMap, HashSet},
    os::fd::AsFd as _,
};

use wayland_client::{Proxy as _, protocol::wl_buffer::WlBuffer};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use super::client::{Client, DirectEvent, SurfaceId};
use crate::rootless::{
    display::HostWork,
    host::{DirectBuffer, DirectFrame, FrameEvent},
};

/// `DRM_FORMAT_MOD_LINEAR`.
const LINEAR: u64 = 0;
/// `DRM_FORMAT_XRGB8888` and `DRM_FORMAT_ARGB8888`, the fourccs.
const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");

/// The fourcc a client buffer of `depth` is.
fn fourcc(depth: u8) -> Option<u32> {
    match depth {
        24 => Some(XRGB8888),
        32 => Some(ARGB8888),
        _ => None,
    }
}

/// The compositor's buffer for a client's.
#[derive(Debug)]
enum Made {
    /// `create` sent, not answered yet.
    Asked(ZwpLinuxBufferParamsV1),
    Ready(WlBuffer),
    /// The compositor cannot use it: its window is read back.
    Refused,
}

/// The host's side of direct frames.
#[derive(Debug)]
pub(super) struct Direct {
    dmabuf: ZwpLinuxDmabufV1,
    /// The formats the compositor named LINEAR.
    formats: HashSet<u32>,
    /// By `DirectBuffer::generation`.
    made: HashMap<u64, Made>,
    /// The generation each object asked or made is for, by its protocol id.
    by_object: HashMap<u32, u64>,
    /// Asked for client buffers that went before the answer, by the params'
    /// protocol id: what the answer makes is destroyed.
    cancelled: HashMap<u32, ZwpLinuxBufferParamsV1>,
}

impl Direct {
    /// Bind the compositor's `zwp_linux_dmabuf_v1` at version 3, where its
    /// formats and modifiers are events; `None` without one at version 3
    /// or later.
    pub(super) fn bind(client: &Client) -> Option<Self> {
        let Some(dmabuf) = client.bind::<ZwpLinuxDmabufV1>(3) else {
            log::info!("wayland: the compositor takes no dma-bufs; every window is read back");
            return None;
        };
        if dmabuf.version() < 3 {
            log::info!(
                "wayland: the compositor's zwp_linux_dmabuf_v1 names no modifiers (version {}); \
                 every window is read back",
                dmabuf.version()
            );
            dmabuf.destroy();
            return None;
        }
        Some(Self {
            dmabuf,
            formats: HashSet::new(),
            made: HashMap::new(),
            by_object: HashMap::new(),
            cancelled: HashMap::new(),
        })
    }

    /// Whether the compositor named a LINEAR format for buffers of `depth`.
    pub(super) fn takes(&self, depth: u8) -> bool {
        fourcc(depth).is_some_and(|format| self.formats.contains(&format))
    }

    /// An event of the objects made here. A `wl_buffer.release` is the
    /// client buffer given back.
    pub(super) fn event(&mut self, event: DirectEvent, work: &mut HostWork) {
        let (params, buffer) = match event {
            DirectEvent::Modifier { format, modifier } => {
                if modifier == LINEAR
                    && (format == XRGB8888 || format == ARGB8888)
                    && self.formats.insert(format)
                {
                    log::info!(
                        "wayland: the compositor takes LINEAR {}; whole-window presents of \
                         such buffers go to it as they are",
                        if format == XRGB8888 {
                            "XRGB8888"
                        } else {
                            "ARGB8888"
                        }
                    );
                }
                return;
            }
            DirectEvent::Released(buffer) => {
                let generation = self.by_object.get(&buffer.id().protocol_id()).copied();
                if let Some(generation) = generation
                    && matches!(self.made.get(&generation), Some(Made::Ready(made)) if *made == buffer)
                {
                    work.frame(FrameEvent::DirectReleased { generation });
                }
                return;
            }
            DirectEvent::Created { params, buffer } => (params, Some(buffer)),
            DirectEvent::Failed(params) => (params, None),
        };
        let id = params.id().protocol_id();
        params.destroy();
        if self.cancelled.remove(&id).is_some() {
            // Asked for a client buffer that went before the answer.
            if let Some(buffer) = buffer {
                buffer.destroy();
            }
            return;
        }
        let Some(generation) = self.by_object.remove(&id) else {
            return;
        };
        let made = match buffer {
            Some(buffer) => {
                let _ = self.by_object.insert(buffer.id().protocol_id(), generation);
                Made::Ready(buffer)
            }
            None => {
                log::warn!(
                    "wayland: the compositor cannot use a client's buffer; its window is read \
                     back instead"
                );
                Made::Refused
            }
        };
        let _ = self.made.insert(generation, made);
    }

    /// The client buffers of `generations` went: the compositor's buffers
    /// of them go too.
    pub(super) fn forget(&mut self, generations: &[u64]) {
        for generation in generations {
            match self.made.remove(generation) {
                Some(Made::Ready(buffer)) => {
                    let _ = self.by_object.remove(&buffer.id().protocol_id());
                    buffer.destroy();
                }
                Some(Made::Asked(params)) => {
                    let id = params.id().protocol_id();
                    let _ = self.by_object.remove(&id);
                    let _ = self.cancelled.insert(id, params);
                }
                Some(Made::Refused) | None => {}
            }
        }
    }

    /// Ask the compositor to make its buffer of a client's: once, and only
    /// in a format it named LINEAR. The descriptor goes with the request
    /// (duplicated into it) and is closed here.
    pub(super) fn import(&mut self, client: &Client, buffer: DirectBuffer) {
        if self.made.contains_key(&buffer.generation) {
            debug_assert!(false, "a client buffer given twice");
            return;
        }
        let Some(format) = fourcc(buffer.depth).filter(|_| self.takes(buffer.depth)) else {
            let _ = self.made.insert(buffer.generation, Made::Refused);
            return;
        };
        debug_assert!(
            buffer.width > 0 && buffer.height > 0,
            "an empty client buffer"
        );
        let params = self.dmabuf.create_params(client.handle(), ());
        // LINEAR: both halves of the modifier zero.
        params.add(buffer.fd.as_fd(), 0, buffer.offset, buffer.stride, 0, 0);
        params.create(
            i32::from(buffer.width),
            i32::from(buffer.height),
            format,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );
        let _ = self
            .by_object
            .insert(params.id().protocol_id(), buffer.generation);
        let _ = self.made.insert(buffer.generation, Made::Asked(params));
    }

    /// Attach the compositor's buffer of `frame` to `surface`, damage all
    /// of it and commit it with a frame callback; whether it was. A frame is
    /// declined until its buffer is made, and for one refused.
    pub(super) fn show(&self, client: &Client, surface: SurfaceId, frame: &DirectFrame) -> bool {
        let Some(Made::Ready(buffer)) = self.made.get(&frame.generation) else {
            return false;
        };
        if !client.attach(
            surface,
            buffer,
            (u32::from(frame.width), u32::from(frame.height)),
        ) {
            return false;
        }
        client.commit_frame(surface);
        true
    }
}

#[cfg(test)]
impl Direct {
    /// The object asked to make the buffer of `generation`, while it is.
    pub(super) fn asked_for_test(&self, generation: u64) -> Option<u32> {
        match self.made.get(&generation)? {
            Made::Asked(params) => Some(params.id().protocol_id()),
            Made::Ready(_) | Made::Refused => None,
        }
    }

    pub(super) fn dmabuf_for_test(&self) -> u32 {
        self.dmabuf.id().protocol_id()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        os::{fd::OwnedFd, unix::net::UnixStream},
    };

    use yserver_core::rootless::ToplevelId;

    use super::*;
    use crate::{
        rootless::display::HostItem,
        wayland::client::{
            ToplevelOptions,
            wire::{global, message, sent},
        },
    };

    /// A tiled modifier (`I915_FORMAT_MOD_X_TILED`): not LINEAR.
    const TILED: u64 = 0x0100_0000_0000_0001;
    /// The first object id a compositor allocates.
    const SERVER_ID: u32 = 0xff00_0000;

    /// A client with a compositor, an xdg shell and dma-bufs at version 4
    /// (bound at 3), a configured 64×48 window, and `Direct` bound.
    fn connected() -> (Client, UnixStream, Direct, SurfaceId) {
        let (host, mut compositor) = UnixStream::pair().unwrap();
        let mut client = Client::for_test(host);
        for (name, interface, version) in [
            (1, "wl_compositor", 6),
            (2, "xdg_wm_base", 3),
            (3, "zwp_linux_dmabuf_v1", 4),
        ] {
            compositor
                .write_all(&global(name, interface, version))
                .unwrap();
        }
        client.dispatch().unwrap();
        let direct = Direct::bind(&client).expect("dma-bufs");
        assert_eq!(
            direct.dmabuf.version(),
            3,
            "formats and modifiers as events"
        );
        let surface = client
            .toplevel(&ToplevelOptions {
                title: String::new(),
                app_id: String::new(),
                size: (64, 48),
                parent: None,
                min_size: (0, 0),
                max_size: (0, 0),
            })
            .unwrap();
        client.configure_for_test(surface, (64, 48));
        client.flush().unwrap();
        let _ = sent(&mut compositor);
        (client, compositor, direct, surface)
    }

    /// `zwp_linux_dmabuf_v1.modifier` events, dispatched into `direct`.
    fn advertise(
        client: &mut Client,
        compositor: &mut UnixStream,
        direct: &mut Direct,
        formats: &[(&[u8; 4], u64)],
    ) {
        let id = direct.dmabuf.id().protocol_id();
        for (format, modifier) in formats {
            let mut arguments = u32::from_le_bytes(**format).to_ne_bytes().to_vec();
            arguments.extend_from_slice(&u32::try_from(modifier >> 32).unwrap().to_ne_bytes());
            arguments
                .extend_from_slice(&u32::try_from(modifier & 0xffff_ffff).unwrap().to_ne_bytes());
            compositor.write_all(&message(id, 1, &arguments)).unwrap();
        }
        dispatch(client, direct);
    }

    /// What the compositor sent, into `direct`; what it made of it.
    fn dispatch(client: &mut Client, direct: &mut Direct) -> Vec<FrameEvent> {
        let mut work = HostWork::default();
        for event in client.dispatch().unwrap() {
            if let super::super::client::Event::Direct(event) = event {
                direct.event(event, &mut work);
            }
        }
        work.into_iter()
            .filter_map(|item| match item {
                HostItem::Frame(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    fn frame(generation: u64) -> DirectFrame {
        DirectFrame {
            id: ToplevelId::fake(0x40_0001),
            generation,
            width: 64,
            height: 48,
        }
    }

    fn buffer(generation: u64, depth: u8) -> DirectBuffer {
        DirectBuffer {
            generation,
            width: 64,
            height: 48,
            offset: 0,
            stride: 256,
            depth,
            fd: OwnedFd::from(std::fs::File::open("/dev/null").unwrap()),
        }
    }

    fn params_of(direct: &Direct, generation: u64) -> u32 {
        match &direct.made[&generation] {
            Made::Asked(params) => params.id().protocol_id(),
            other => panic!("not asked: {other:?}"),
        }
    }

    /// A client's buffer is made with one `add` and `create`, never
    /// `create_immed`, whose import failure may end the connection; the frame
    /// is declined until the compositor made it, and then attached, damaged
    /// whole and committed with a frame callback. Its release is the
    /// client's buffer back, and its going destroys the compositor's.
    #[test]
    fn a_buffer_is_made_with_create_and_shown_once_made() {
        let (mut client, mut compositor, mut direct, surface) = connected();
        advertise(
            &mut client,
            &mut compositor,
            &mut direct,
            &[(b"XR24", LINEAR)],
        );
        direct.import(&client, buffer(1, 24));
        assert!(!direct.show(&client, surface, &frame(1)), "not made yet");
        client.flush().unwrap();
        let dmabuf = direct.dmabuf.id().protocol_id();
        let params = params_of(&direct, 1);
        let requests: Vec<(u32, u16)> = sent(&mut compositor)
            .into_iter()
            .map(|(object, opcode, _)| (object, opcode))
            .collect();
        assert_eq!(
            requests,
            [
                (dmabuf, 1), // create_params
                (params, 1), // add
                (params, 2), // create, not create_immed (3)
            ]
        );
        // created: the compositor's buffer.
        compositor
            .write_all(&message(params, 0, &SERVER_ID.to_ne_bytes()))
            .unwrap();
        assert!(dispatch(&mut client, &mut direct).is_empty());
        assert!(direct.show(&client, surface, &frame(1)));
        client.flush().unwrap();
        let window = client.surface(surface).unwrap().id().protocol_id();
        let requests = sent(&mut compositor);
        let ops: Vec<(u32, u16)> = requests.iter().map(|(o, op, _)| (*o, *op)).collect();
        assert_eq!(
            ops,
            [
                (params, 0), // zwp_linux_buffer_params_v1.destroy
                (window, 1), // wl_surface.attach
                (window, 9), // damage_buffer
                (window, 3), // frame
                (window, 6), // commit
            ]
        );
        let attach = &requests[1].2;
        assert_eq!(
            attach[..4],
            SERVER_ID.to_ne_bytes(),
            "the compositor's buffer"
        );
        let damage: Vec<u8> = [0u32, 0, 64, 48]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        assert_eq!(requests[2].2, damage, "all of it");
        // wl_buffer.release.
        compositor.write_all(&message(SERVER_ID, 0, &[])).unwrap();
        let events = dispatch(&mut client, &mut direct);
        assert!(
            matches!(
                events.as_slice(),
                [FrameEvent::DirectReleased { generation: 1 }]
            ),
            "{events:?}"
        );
        direct.forget(&[1]);
        client.flush().unwrap();
        let requests: Vec<(u32, u16)> = sent(&mut compositor)
            .into_iter()
            .map(|(object, opcode, _)| (object, opcode))
            .collect();
        assert_eq!(requests, [(SERVER_ID, 0)], "wl_buffer.destroy");
    }

    /// Only a format the compositor named LINEAR is sent: nothing before it
    /// named any, nothing in a format it named only tiled.
    #[test]
    fn a_format_the_compositor_did_not_name_linear_is_not_sent() {
        let (mut client, mut compositor, mut direct, surface) = connected();
        assert!(!direct.takes(24));
        direct.import(&client, buffer(1, 24));
        advertise(
            &mut client,
            &mut compositor,
            &mut direct,
            &[(b"XR24", LINEAR), (b"AR24", TILED)],
        );
        assert!(direct.takes(24) && !direct.takes(32) && !direct.takes(16));
        direct.import(&client, buffer(2, 32));
        direct.import(&client, buffer(3, 16));
        for generation in 1..=3 {
            assert!(!direct.show(&client, surface, &frame(generation)));
        }
        client.flush().unwrap();
        assert!(sent(&mut compositor).is_empty(), "nothing asked for");
        assert!(
            direct
                .made
                .values()
                .all(|made| matches!(made, Made::Refused))
        );
    }

    /// A buffer the compositor cannot use is declined from then on, and not
    /// asked for again.
    #[test]
    fn a_buffer_the_compositor_refuses_is_not_asked_for_again() {
        let (mut client, mut compositor, mut direct, surface) = connected();
        advertise(
            &mut client,
            &mut compositor,
            &mut direct,
            &[(b"XR24", LINEAR)],
        );
        direct.import(&client, buffer(1, 24));
        client.flush().unwrap();
        let params = params_of(&direct, 1);
        let _ = sent(&mut compositor);
        // failed.
        compositor.write_all(&message(params, 1, &[])).unwrap();
        dispatch(&mut client, &mut direct);
        assert!(!direct.show(&client, surface, &frame(1)));
        client.flush().unwrap();
        let requests: Vec<(u32, u16)> = sent(&mut compositor)
            .into_iter()
            .map(|(object, opcode, _)| (object, opcode))
            .collect();
        assert_eq!(requests, [(params, 0)], "only the params destroyed");
    }

    /// A client buffer that goes before the compositor answered: what the
    /// answer makes is destroyed.
    #[test]
    fn a_buffer_that_goes_before_it_is_made_is_destroyed_when_made() {
        let (mut client, mut compositor, mut direct, _) = connected();
        advertise(
            &mut client,
            &mut compositor,
            &mut direct,
            &[(b"XR24", LINEAR)],
        );
        direct.import(&client, buffer(1, 24));
        client.flush().unwrap();
        let params = params_of(&direct, 1);
        let _ = sent(&mut compositor);
        direct.forget(&[1]);
        compositor
            .write_all(&message(params, 0, &SERVER_ID.to_ne_bytes()))
            .unwrap();
        dispatch(&mut client, &mut direct);
        client.flush().unwrap();
        let requests: Vec<(u32, u16)> = sent(&mut compositor)
            .into_iter()
            .map(|(object, opcode, _)| (object, opcode))
            .collect();
        assert_eq!(requests, [(params, 0), (SERVER_ID, 0)]);
        assert!(direct.made.is_empty() && direct.cancelled.is_empty());
    }
}
