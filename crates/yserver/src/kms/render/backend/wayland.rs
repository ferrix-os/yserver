//! The rootless Wayland backend's side of `KmsBackend` (`crate::wayland`):
//! the server's own redirect of the root's children, and each top-level's
//! image read back for the compositor.

use super::*;

impl KmsBackend {
    /// The Wayland backend's turn of each loop iteration: keep the root's
    /// children redirected for the server, so each top-level's image is one
    /// backing, then bring the compositor's windows in line with them
    /// (`crate::wayland::WaylandLink::sync`).
    pub(in crate::kms::render::backend) fn sync_wayland(&mut self, state: &mut ServerState) {
        if self.wayland.is_none() {
            return;
        }
        let redirected = yserver_core::core_loop::process_request::redirect_subwindows_for_server(
            state,
            self,
            yserver_core::resources::ROOT_WINDOW,
            yserver_core::server::CompositeRedirectMode::Manual,
        );
        if !redirected && !self.wayland_redirect_refused {
            // A client redirected the root's children first. The windows
            // still show, but without their subwindows.
            log::warn!("wayland: a client redirects the root's children; subwindows will not show");
        }
        self.wayland_redirect_refused = !redirected;
        // The compositor manages the windows, so clients asking which
        // window manager does are told its name; of the EWMH hints, the
        // title is carried over to it (`crate::wayland`'s `names`), and a
        // drag by a window's own title bar or edge
        // (`WaylandLink::wm_request`).
        if let Err(error) =
            yserver_core::core_loop::process_request::announce_window_manager_for_server(
                state,
                self,
                "hyprix",
                &[
                    "_NET_SUPPORTING_WM_CHECK",
                    "_NET_WM_NAME",
                    "_NET_WM_MOVERESIZE",
                ],
            )
        {
            log::warn!("wayland: announcing the window manager: {error}");
        }
        let Some(mut link) = self.wayland.take() else {
            return;
        };
        link.sync(state, self);
        for request in std::mem::take(&mut state.server_wm_requests) {
            link.wm_request(state, request);
        }
        link.sync_cursor(self);
        let requests = link.take_requests();
        link.sync_clipboard(state, self);
        self.wayland = Some(link);
        // What the compositor asked of the windows, which only the core,
        // with the state in hand, can carry out.
        for request in requests {
            let done = match request {
                crate::wayland::Request::Resize {
                    window,
                    width,
                    height,
                } => yserver_core::core_loop::process_request::configure_window_for_server(
                    state, self, window, width, height,
                ),
                crate::wayland::Request::Close { window } => {
                    yserver_core::core_loop::process_request::close_window_for_server(
                        state, self, window,
                    )
                }
            };
            if let Err(error) = done {
                log::warn!("wayland: {request:?}: {error}");
            }
        }
    }
}

/// The cursor in effect, for the Wayland backend to give the compositor.
impl crate::wayland::CursorSource for KmsBackend {
    fn cursor_key(&self) -> Option<(u32, u64)> {
        let xid = self.effective_cursor_xid?;
        Some((xid, self.cursor_records.get(&xid)?.version))
    }

    fn cursor_image(&self) -> Option<yserver_core::backend::ActiveCursorImage> {
        Backend::get_active_cursor_image(self)
    }
}

/// Each top-level's image for the Wayland backend: the window read through
/// its redirect routing, which under the server's own redirect of the root's
/// children is its backing, subwindows included.
impl crate::wayland::WindowImages for KmsBackend {
    fn image_version(&self, host_xid: u32) -> Option<u64> {
        let target = self.resolve_paint_target(host_xid)?;
        self.store
            .get(target.backing_id())
            .map(|drawable| drawable.content_version)
    }

    fn read_image_into(
        &mut self,
        host_xid: u32,
        width: u16,
        height: u16,
        border: u16,
        out: &mut [u8],
    ) -> bool {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return false;
        };
        let depth = target.x11_depth();
        if depth != 24 && depth != 32 {
            return false;
        }
        let border = u32::from(border);
        let (width, height) = (
            u32::from(width) + 2 * border,
            u32::from(height) + 2 * border,
        );
        let stride = width as usize * 4;
        if out.len() < stride * height as usize {
            return false;
        }
        let inset = i32::try_from(border).unwrap_or(0);
        let rect = vk::Rect2D {
            offset: vk::Offset2D {
                x: target.offset().0 - inset,
                y: target.offset().1 - inset,
            },
            extent: vk::Extent2D { width, height },
        };
        // Storage at depth 24 and 32 is BGRA8, which is the compositor's
        // buffer's own layout: rows copied as they are. Short only while a
        // resize is under way, when what the backing lacks is black.
        let readback = self.engine.get_image_with(
            &mut self.store,
            &mut self.platform,
            target.src_including_border(),
            rect,
            depth,
            |raw, copy_w, copy_h| {
                let row = (copy_w as usize * 4).min(stride);
                if copy_w as usize * 4 == stride && copy_h == height {
                    out[..raw.len()].copy_from_slice(raw);
                    return Ok(());
                }
                for (y, to) in out
                    .chunks_exact_mut(stride)
                    .take(height as usize)
                    .enumerate()
                {
                    match raw
                        .get(y * copy_w as usize * 4..)
                        .and_then(|from| from.get(..row))
                    {
                        Some(from) if y < copy_h as usize => {
                            to[..row].copy_from_slice(from);
                            to[row..].fill(0);
                        }
                        _ => to.fill(0),
                    }
                }
                Ok(())
            },
        );
        self.drain_frame_builder_telemetry();
        match readback {
            Ok(()) => true,
            Err(error) => {
                log::warn!("wayland: reading window 0x{host_xid:x} back: {error:?}");
                false
            }
        }
    }
}
