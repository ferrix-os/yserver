//! Direct frames on Wayland: a client's LINEAR DRI3 buffer handed to the
//! compositor as a `zwp_linux_dmabuf_v1` buffer, instead of read back into
//! `wl_shm` (`RootlessHost::submit_direct`).
//!
//! The compositor's buffer for a client buffer is made once per buffer with
//! `zwp_linux_buffer_params_v1.create` (never `create_immed`, whose failure
//! ends the connection) and kept until the platform says the client buffer
//! went (`forget_direct`). A frame whose buffer is not made yet, or was
//! refused, is declined, and the window is read back as before.
//!
//! `YSERVER_WAYLAND_DIRECT=0` keeps every window read back.

use std::{
    collections::HashMap,
    os::fd::{AsFd as _, OwnedFd},
    time::{Duration, Instant},
};

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use super::client::{Client, DirectEvent, SurfaceId};
use crate::rootless::{
    display::HostWork,
    host::{DirectFrame, FrameEvent},
};

/// How often the count of direct frames is logged.
const REPORT: Duration = Duration::from_secs(10);

/// A buffer of the compositor's for a client's.
enum Made {
    /// `create` sent; the descriptor kept until it is answered.
    Asked {
        params: ZwpLinuxBufferParamsV1,
        _fd: OwnedFd,
    },
    /// The compositor's buffer.
    Ready(WlBuffer),
    /// Refused: the window is read back instead.
    Refused,
}

/// The host's side of direct frames.
pub(super) struct Direct {
    dmabuf: ZwpLinuxDmabufV1,
    /// By `DirectFrame::generation`.
    made: HashMap<u64, Made>,
    frames: u64,
    reported_frames: u64,
    reported: Instant,
}

impl Direct {
    /// Bind the compositor's `zwp_linux_dmabuf_v1`; `None` without one, or
    /// with `YSERVER_WAYLAND_DIRECT=0`.
    pub(super) fn bind(client: &Client) -> Option<Self> {
        if std::env::var("YSERVER_WAYLAND_DIRECT").is_ok_and(|value| value == "0") {
            log::info!("wayland: YSERVER_WAYLAND_DIRECT=0; every window is read back");
            return None;
        }
        match client.bind::<ZwpLinuxDmabufV1>(3) {
            Some(dmabuf) => {
                log::info!(
                    "wayland: the compositor takes dmabufs; whole-window presents go to it as they are"
                );
                Some(Self {
                    dmabuf,
                    made: HashMap::new(),
                    frames: 0,
                    reported_frames: 0,
                    reported: Instant::now(),
                })
            }
            None => {
                log::info!("wayland: the compositor takes no dmabufs; every window is read back");
                None
            }
        }
    }

    /// An event of the objects made here. A `wl_buffer.release` is the
    /// client buffer back.
    pub(super) fn event(&mut self, event: DirectEvent, work: &mut HostWork) {
        let (params, buffer) = match event {
            DirectEvent::Released(buffer) => {
                if let Some((&generation, _)) = self
                    .made
                    .iter()
                    .find(|(_, made)| matches!(made, Made::Ready(made) if *made == buffer))
                {
                    work.frame(FrameEvent::DirectReleased { generation });
                }
                return;
            }
            DirectEvent::Created { params, buffer } => (params, Some(buffer)),
            DirectEvent::Failed(params) => (params, None),
        };
        params.destroy();
        let Some((&generation, _)) = self.made.iter().find(
            |(_, made)| matches!(made, Made::Asked { params: asked, .. } if *asked == params),
        ) else {
            // Asked for a client buffer that went before the answer.
            if let Some(buffer) = buffer {
                buffer.destroy();
            }
            return;
        };
        let made = match buffer {
            Some(buffer) => Made::Ready(buffer),
            None => {
                log::warn!(
                    "wayland: the compositor refused a client's buffer; its window is read back"
                );
                Made::Refused
            }
        };
        let _ = self.made.insert(generation, made);
    }

    /// Destroy the buffers of client buffers that went.
    pub(super) fn forget(&mut self, generations: &[u64]) {
        for generation in generations {
            match self.made.remove(generation) {
                Some(Made::Ready(buffer)) => buffer.destroy(),
                Some(Made::Asked { params, .. }) => params.destroy(),
                _ => {}
            }
        }
    }

    /// The compositor's buffer for `frame`'s, asked for the first time;
    /// `None` until it is made, and for one refused.
    fn buffer(&mut self, client: &Client, frame: DirectFrame) -> Option<WlBuffer> {
        match self.made.get(&frame.generation) {
            Some(Made::Ready(buffer)) => return Some(buffer.clone()),
            Some(_) => return None,
            None => {}
        }
        let format = if frame.depth == 32 {
            u32::from_le_bytes(*b"AR24")
        } else {
            u32::from_le_bytes(*b"XR24")
        };
        let [high, low] = [
            u32::try_from(frame.modifier >> 32).unwrap_or(0),
            u32::try_from(frame.modifier & 0xffff_ffff).unwrap_or(0),
        ];
        let params = self.dmabuf.create_params(client.handle(), ());
        params.add(frame.fd.as_fd(), 0, frame.offset, frame.stride, high, low);
        params.create(
            i32::from(frame.width),
            i32::from(frame.height),
            format,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );
        let _ = self.made.insert(
            frame.generation,
            Made::Asked {
                params,
                _fd: frame.fd,
            },
        );
        None
    }

    /// Attach `frame`'s buffer to `surface` and commit it; whether it was.
    pub(super) fn show(&mut self, client: &Client, surface: SurfaceId, frame: DirectFrame) -> bool {
        let (width, height, serial) = (frame.width, frame.height, frame.serial);
        let Some(buffer) = self.buffer(client, frame) else {
            return false;
        };
        if !client.attach(surface, &buffer, (u32::from(width), u32::from(height))) {
            return false;
        }
        client.commit_frame(surface);
        self.frames += 1;
        log::debug!(target: "present_pace", "WL-INSTR stage=commit serial={serial}");
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
