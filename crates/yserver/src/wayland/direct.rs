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
    os::fd::{AsRawFd, OwnedFd},
    time::{Duration, Instant},
};

use compositor_toolkit::{
    Client, ObjectId, SurfaceId, Value,
    protocol::{
        core::{self as wl, wl_surface},
        linux_dmabuf::{self, zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1},
    },
};

use crate::rootless::host::{DirectFrame, FrameEvent};

/// How often the count of direct frames is logged.
const REPORT: Duration = Duration::from_secs(10);

/// A buffer of the compositor's for a client's.
enum Made {
    /// `create` sent; the descriptor kept until it is answered.
    Asked { params: ObjectId, _fd: OwnedFd },
    /// The compositor's buffer.
    Ready(ObjectId),
    /// Refused: the window is read back instead.
    Refused,
}

/// The host's side of direct frames.
pub(super) struct Direct {
    dmabuf: ObjectId,
    /// By `DirectFrame::generation`.
    made: HashMap<u64, Made>,
    frames: u64,
    reported_frames: u64,
    reported: Instant,
}

impl Direct {
    /// Bind the compositor's `zwp_linux_dmabuf_v1`; `None` without one, or
    /// with `YSERVER_WAYLAND_DIRECT=0`.
    pub(super) fn bind(client: &mut Client) -> Option<Self> {
        if std::env::var("YSERVER_WAYLAND_DIRECT").is_ok_and(|value| value == "0") {
            log::info!("wayland: YSERVER_WAYLAND_DIRECT=0; every window is read back");
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

    /// An event of the objects made here; whether it was one. A
    /// `wl_buffer.release` is the client buffer back.
    pub(super) fn event(
        &mut self,
        client: &mut Client,
        object: ObjectId,
        opcode: u16,
        args: &[Value],
        frames: &mut Vec<FrameEvent>,
    ) -> bool {
        if opcode == wl::wl_buffer::event::RELEASE
            && let Some((&generation, _)) = self
                .made
                .iter()
                .find(|(_, made)| matches!(made, Made::Ready(buffer) if *buffer == object))
        {
            frames.push(FrameEvent::DirectReleased { generation });
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

    /// Destroy the buffers of client buffers that went.
    pub(super) fn forget(&mut self, client: &mut Client, generations: &[u64]) {
        for generation in generations {
            match self.made.remove(generation) {
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
    fn buffer(&mut self, client: &mut Client, frame: DirectFrame) -> Option<ObjectId> {
        match self.made.get(&frame.generation) {
            Some(Made::Ready(buffer)) => return Some(*buffer),
            Some(_) => return None,
            None => {}
        }
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
        let [high, low] = [
            u32::try_from(frame.modifier >> 32).unwrap_or(0),
            u32::try_from(frame.modifier & 0xffff_ffff).unwrap_or(0),
        ];
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
                        Value::Fd(frame.fd.as_raw_fd()),
                        Value::Uint(0),
                        Value::Uint(frame.offset),
                        Value::Uint(frame.stride),
                        Value::Uint(high),
                        Value::Uint(low),
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
            Ok(()) => Made::Asked {
                params,
                _fd: frame.fd,
            },
            Err(error) => {
                log::warn!("wayland: handing the compositor a client's buffer: {error}");
                Made::Refused
            }
        };
        let _ = self.made.insert(frame.generation, made);
        None
    }

    /// Attach `frame`'s buffer to `surface` and commit it; whether it was.
    pub(super) fn show(
        &mut self,
        client: &mut Client,
        surface: SurfaceId,
        frame: DirectFrame,
    ) -> bool {
        let (width, height, serial) = (frame.width, frame.height, frame.serial);
        let Some(buffer) = self.buffer(client, frame) else {
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
                        Value::Int(i32::from(width)),
                        Value::Int(i32::from(height)),
                    ],
                )
            });
        if let Err(error) = attached {
            log::warn!("wayland: attaching a client's buffer: {error}");
            return false;
        }
        // Commits what was attached, as the surface has been drawn.
        client.request_frame(surface);
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
