//! Zero-copy windows: a game's DRI3 buffer handed to the compositor as a
//! `zwp_linux_dmabuf_v1` buffer instead of read back into `wl_shm`.
//!
//! The renderer side (`kms/render/wl_direct.rs`) owes a window's backing a
//! whole-window present of a LINEAR DRI3 buffer instead of copying it, and
//! [`WindowImages::direct_frame`] says so. Here such a frame is attached as
//! the compositor's buffer for the client's buffer, made once per buffer
//! with `zwp_linux_buffer_params_v1.create` (never `create_immed`, whose
//! failure ends the connection) and kept until its pixmap goes. A window
//! whose frame cannot go direct -- the buffer not made yet, or refused, or
//! other windows drawn inside it -- is read back as before, which settles
//! what its backing is owed.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use compositor_toolkit::protocol::core::{self as wl, wl_surface};
use compositor_toolkit::protocol::linux_dmabuf::{
    self, zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use compositor_toolkit::{Client, ObjectId, SurfaceId, Value};
use yserver_core::resources::{MapState, Window};
use yserver_core::server::ServerState;

use super::{Frames, WindowImages};

/// How often the count of zero-copy frames is logged.
const REPORT: Duration = Duration::from_secs(10);

/// A window's frame that is a client's buffer, as the renderer owes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectFrame {
    /// The buffer's, unique for the server's life.
    pub generation: u64,
    /// The present's, new for each.
    pub serial: u64,
    /// The window it was presented to (the top-level or one inside it).
    pub dst_host_xid: u32,
    /// Its size.
    pub width: u16,
    /// Its size.
    pub height: u16,
    /// Where its one plane starts in the buffer.
    pub offset: u32,
    /// Its rows' pitch.
    pub stride: u32,
    /// The pixmap's depth, 24 or 32.
    pub depth: u8,
    /// The second of a pair: not handed over before this `CLOCK_MONOTONIC`
    /// microsecond (`kms/render/wl_direct.rs`'s pacing).
    pub not_before: Option<u64>,
}

/// A buffer of the compositor's for a client's.
enum Made {
    /// `create` sent; the descriptor kept until it is answered.
    Asked { params: ObjectId, _fd: OwnedFd },
    /// The compositor's buffer.
    Ready(ObjectId),
    /// Refused: the window is read back instead.
    Refused,
}

/// The link's side of zero-copy windows.
pub(super) struct Direct {
    dmabuf: ObjectId,
    /// By [`DirectFrame::generation`].
    made: HashMap<u64, Made>,
    /// Generations whose buffer the compositor gave back since asked.
    released: Vec<u64>,
    frames: u64,
    reported_frames: u64,
    reported: Instant,
}

impl Direct {
    /// Bind the compositor's `zwp_linux_dmabuf_v1`; `None` without one, or
    /// with `YSERVER_WAYLAND_DIRECT=0`.
    pub(super) fn bind(client: &mut Client) -> Option<Self> {
        if std::env::var("YSERVER_WAYLAND_DIRECT").is_ok_and(|value| value == "0") {
            return None;
        }
        match client.bind(&linux_dmabuf::ZWP_LINUX_DMABUF_V1, 3, None) {
            Ok((dmabuf, _)) => {
                log::info!(
                    "wayland: the compositor takes dmabufs; whole-window presents go to it as they are"
                );
                Some(Self {
                    dmabuf,
                    made: HashMap::new(),
                    released: Vec::new(),
                    frames: 0,
                    reported_frames: 0,
                    reported: Instant::now(),
                })
            }
            Err(_) => {
                log::info!("wayland: the compositor takes no dmabufs; every window is read back");
                None
            }
        }
    }

    /// An event of the objects made here; whether it was one.
    pub(super) fn event(
        &mut self,
        client: &mut Client,
        object: ObjectId,
        opcode: u16,
        args: &[Value],
    ) -> bool {
        // `wl_buffer.release`: the compositor is done reading the client's
        // buffer, which may be drawn into again.
        if opcode == wl::wl_buffer::event::RELEASE
            && let Some((&generation, _)) = self
                .made
                .iter()
                .find(|(_, made)| matches!(made, Made::Ready(buffer) if *buffer == object))
        {
            self.released.push(generation);
            return true;
        }
        let Some((&generation, _)) = self
            .made
            .iter()
            .find(|(_, made)| matches!(made, Made::Asked { params, .. } if *params == object))
        else {
            return false;
        };
        let made = match (opcode, args.first()) {
            (
                zwp_linux_buffer_params_v1::event::CREATED,
                Some(Value::Object(buffer) | Value::NewId(buffer)),
            ) => Made::Ready(*buffer),
            _ => {
                log::warn!(
                    "wayland: the compositor refused a client's buffer; its window is read back"
                );
                Made::Refused
            }
        };
        let _ = client.request(object, zwp_linux_buffer_params_v1::request::DESTROY, &[]);
        let _ = self.made.insert(generation, made);
        true
    }

    /// The generations whose buffer the compositor gave back since last
    /// asked.
    pub(super) fn take_released(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.released)
    }

    /// Destroy the buffers of client buffers that went.
    pub(super) fn forget(&mut self, client: &mut Client, generations: Vec<u64>) {
        for generation in generations {
            match self.made.remove(&generation) {
                Some(Made::Ready(buffer)) => {
                    let _ = client.request(buffer, wl::wl_buffer::request::DESTROY, &[]);
                }
                Some(Made::Asked { params, .. }) => {
                    let _ =
                        client.request(params, zwp_linux_buffer_params_v1::request::DESTROY, &[]);
                }
                _ => {}
            }
        }
    }

    /// The compositor's buffer for `frame`'s, asked for the first time;
    /// `None` until it is made, and for one refused.
    fn buffer(
        &mut self,
        client: &mut Client,
        images: &dyn WindowImages,
        frame: &DirectFrame,
    ) -> Option<ObjectId> {
        match self.made.get(&frame.generation) {
            Some(Made::Ready(buffer)) => return Some(*buffer),
            Some(_) => return None,
            None => {}
        }
        let fd = images.direct_fd(frame.generation)?;
        let params = client.new_object(&linux_dmabuf::ZWP_LINUX_BUFFER_PARAMS_V1, 3);
        client.adopt_new_ids(
            params,
            zwp_linux_buffer_params_v1::event::CREATED,
            &wl::WL_BUFFER,
            1,
        );
        let format = if frame.depth == 32 {
            u32::from_le_bytes(*b"AR24")
        } else {
            u32::from_le_bytes(*b"XR24")
        };
        let sent = client
            .request(
                self.dmabuf,
                zwp_linux_dmabuf_v1::request::CREATE_PARAMS,
                &[Value::NewId(params)],
            )
            .and_then(|()| {
                client.request(
                    params,
                    zwp_linux_buffer_params_v1::request::ADD,
                    &[
                        Value::Fd(fd.as_raw_fd()),
                        Value::Uint(0),
                        Value::Uint(frame.offset),
                        Value::Uint(frame.stride),
                        // DRM_FORMAT_MOD_LINEAR.
                        Value::Uint(0),
                        Value::Uint(0),
                    ],
                )
            })
            .and_then(|()| {
                client.request(
                    params,
                    zwp_linux_buffer_params_v1::request::CREATE,
                    &[
                        Value::Int(i32::from(frame.width)),
                        Value::Int(i32::from(frame.height)),
                        Value::Uint(format),
                        Value::Uint(0),
                    ],
                )
            });
        let made = match sent {
            Ok(()) => Made::Asked { params, _fd: fd },
            Err(error) => {
                log::warn!("wayland: handing the compositor a client's buffer: {error}");
                Made::Refused
            }
        };
        let _ = self.made.insert(frame.generation, made);
        None
    }

    /// Show `window`'s frame as the client's own buffer when it is one the
    /// compositor can be handed; whether it was handled (shown, or already
    /// showing). `false` leaves the window to be read back.
    pub(super) fn show(
        &mut self,
        client: &mut Client,
        images: &mut dyn WindowImages,
        state: &ServerState,
        surface: SurfaceId,
        (host_xid, window): (u32, &Window),
        frames: &mut Frames,
    ) -> bool {
        if frames.pending
            || client.size(surface).is_none()
            || frames.shown.is_none() && frames.direct.is_none()
        {
            // The first frame is read back: the toolkit commits a frame
            // request only once it has drawn the surface.
            return false;
        }
        let Some(frame) = images.direct_frame(host_xid) else {
            return false;
        };
        if (frame.width, frame.height) != (window.width, window.height)
            || !covers(state, window, frame.dst_host_xid)
        {
            return false;
        }
        if frames.direct == Some(frame.serial) && !frames.configured {
            return true;
        }
        if frames.direct != Some(frame.serial)
            && let Some(at) = frame.not_before
            && super::clock::monotonic_us() < at
        {
            // The second of a pair, until its time: the first stays shown.
            return true;
        }
        let Some(buffer) = self.buffer(client, images, &frame) else {
            return false;
        };
        let Some(object) = client.surface_object(surface) else {
            return false;
        };
        let attached = client
            .request(
                object,
                wl_surface::request::ATTACH,
                &[Value::Object(buffer), Value::Int(0), Value::Int(0)],
            )
            .and_then(|()| {
                client.request(
                    object,
                    wl_surface::request::DAMAGE,
                    &[
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(i32::from(frame.width)),
                        Value::Int(i32::from(frame.height)),
                    ],
                )
            });
        if let Err(error) = attached {
            log::warn!("wayland: attaching a client's buffer to 0x{host_xid:x}: {error}");
            return false;
        }
        // Commits what was attached, as the surface has been drawn.
        client.request_frame(surface);
        frames.pending = true;
        frames.direct_pending = true;
        frames.direct = Some(frame.serial);
        images.direct_handed(host_xid, frame.serial);
        // What the compositor shows is no longer the read-back image: the
        // next one read back is handed over whole.
        frames.shown = None;
        frames.rows.clear();
        frames.configured = false;
        self.frames += 1;
        log::debug!(
            target: "present_pace",
            "WL-INSTR t={} stage=commit serial={} window=0x{host_xid:x}",
            yserver_core::core_loop::process_request::pace_instr_ms(),
            frame.serial
        );
        if self.reported.elapsed() >= REPORT {
            log::info!(
                "yserver: zero-copy frames {} ({} in all)",
                self.frames - self.reported_frames,
                self.frames
            );
            self.reported_frames = self.frames;
            self.reported = Instant::now();
        }
        true
    }
}

/// Whether the window `dst` covers the top-level `top` alone: it is `top`,
/// or the one viewable child at each level down to it, each at the origin
/// with its parent's size and no border, and nothing viewable inside it.
fn covers(state: &ServerState, top: &Window, dst: u32) -> bool {
    let mut window = top;
    loop {
        let mut viewable = state
            .resources
            .children(window.id)
            .iter()
            .filter_map(|&child| state.resources.window(child))
            .filter(|child| child.map_state != MapState::Unmapped);
        let first = viewable.next();
        if window.host_xid.map(|handle| handle.as_raw()) == Some(dst) {
            return first.is_none();
        }
        let (Some(child), None) = (first, viewable.next()) else {
            return false;
        };
        if (
            child.x,
            child.y,
            child.width,
            child.height,
            child.border_width,
        ) != (0, 0, window.width, window.height, 0)
        {
            return false;
        }
        window = child;
    }
}
