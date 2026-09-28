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
        let Some(mut link) = self.wayland.take() else {
            return;
        };
        link.sync(state, self);
        self.wayland = Some(link);
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

    fn read_image(&mut self, host_xid: u32, width: u16, height: u16) -> Option<Vec<u8>> {
        let target = self.resolve_paint_target(host_xid)?;
        let depth = target.x11_depth();
        if depth != 24 && depth != 32 {
            return None;
        }
        // The content, inside the border: `get_image`'s rectangle at the
        // window's own origin.
        let rect = vk::Rect2D {
            offset: vk::Offset2D {
                x: target.offset().0,
                y: target.offset().1,
            },
            extent: vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        let readback = self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            target.src_including_border(),
            rect,
            depth,
        );
        self.drain_frame_builder_telemetry();
        match readback {
            Ok(mut bytes) => {
                // Short only while a resize is under way: pad, as
                // `get_image` does, so the rows stay where they belong.
                bytes.resize(usize::from(width) * usize::from(height) * 4, 0);
                Some(bytes)
            }
            Err(error) => {
                log::warn!("wayland: reading window 0x{host_xid:x} back: {error:?}");
                None
            }
        }
    }
}
