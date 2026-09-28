//! The rootless Wayland backend's half of `KmsBackend` (`crate::wayland`):
//! the compositor connection on the backend state, and the RandR topology
//! it implies.

use std::io;

use yserver_core::server::ServerState;

use crate::kms::render::{backend::KmsBackend, scene::OutputSink as _};

impl KmsBackend {
    /// Become a client of a Wayland compositor (`crate::wayland`): take its
    /// first screen's size as the virtual screen's, and keep the connection
    /// for the core loop to poll. Called once, before the topology is read.
    ///
    /// # Errors
    ///
    /// Resizing the virtual screen failing.
    pub fn attach_wayland(&mut self, mut link: crate::wayland::WaylandLink) -> io::Result<()> {
        link.adopt_keyboard_layout(&mut self.core);
        if let Some((width, height)) = link.screen_size() {
            self.apply_virtual_screen_extent(width, height)?;
            log::info!("wayland: the root window is the compositor's screen, {width}x{height}");
        } else {
            log::warn!("wayland: the compositor described no screen; the root stays as it is");
        }
        self.pstate.wayland = Some(link);
        Ok(())
    }

    /// The Wayland backend's RandR topology: one connected output,
    /// `WAYLAND-1`, covering the virtual screen, which is the compositor's
    /// screen (`attach_wayland`). The X screen's size is derived from the
    /// outputs, so without it the root would stay 0×0.
    pub(in crate::kms::render::backend) fn wayland_randr_outputs_and_modes(
        &mut self,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        self.reserve_randr_provider_ids();
        let alloc = &mut self.pstate.randr_id_alloc;
        let (output_id, crtc_id, mode_id) = *self
            .pstate
            .wayland_randr_ids
            .get_or_insert_with(|| (alloc.fresh(), alloc.fresh(), alloc.fresh()));
        let (width, height) = self.platform.fb_dimensions();
        let outputs = vec![yserver_core::randr::RandrOutput {
            name: "WAYLAND-1".to_string(),
            output_id,
            crtc_id,
            mode_id,
            connected: true,
            x: 0,
            y: 0,
            width,
            height,
            vrefresh: 60,
            timing: None,
            // No EDID: `RandrState::output_info` synthesises 96 DPI.
            mm_width: 0,
            mm_height: 0,
            mode_ids: vec![mode_id],
            num_preferred: 1,
            pending_transform: Default::default(),
            current_transform: Default::default(),
            rotation: yserver_core::randr::RR_ROTATE_0,
        }];
        let modes = yserver_core::randr::RandrState::from_outputs(0, outputs.clone()).mode_table;
        (outputs, modes)
    }

    /// The compositor's connection is readable: read and act on its events.
    /// `false` when the compositor closed it.
    pub(in crate::kms::render::backend) fn wayland_on_ready(
        &mut self,
        state: &mut ServerState,
    ) -> bool {
        let Some(mut link) = self.pstate.wayland.take() else {
            return true;
        };
        let alive = match link.dispatch() {
            Ok(()) => {
                link.deliver_input(state, self);
                link.flush();
                true
            }
            Err(error) => {
                log::warn!("wayland: {error}");
                false
            }
        };
        self.pstate.wayland = Some(link);
        alive
    }

    /// The Wayland backend's turn of each loop iteration: keep the root's
    /// children redirected for the server, so each top-level's image is one
    /// backing, then bring the compositor's windows in line with them
    /// (`crate::wayland::WaylandLink::sync`).
    pub(in crate::kms::render::backend) fn sync_wayland(&mut self, state: &mut ServerState) {
        if self.pstate.wayland.is_none() {
            return;
        }
        let redirected = yserver_core::core_loop::process_request::redirect_subwindows_for_server(
            state,
            self,
            yserver_core::resources::ROOT_WINDOW,
            yserver_core::server::CompositeRedirectMode::Manual,
        );
        if !redirected && !self.pstate.wayland_redirect_refused {
            // A client redirected the root's children first. The windows
            // still show, but without their subwindows.
            log::warn!("wayland: a client redirects the root's children; subwindows will not show");
        }
        self.pstate.wayland_redirect_refused = !redirected;
        let Some(mut link) = self.pstate.wayland.take() else {
            return;
        };
        link.sync(state, self);
        link.sync_cursor(self);
        let requests = link.take_requests();
        self.pstate.wayland = Some(link);
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

    /// An X property changed on `host_xid`: the compositor's title and app
    /// id follow `_NET_WM_NAME`, `WM_NAME` and `WM_CLASS`.
    pub(in crate::kms::render::backend) fn wayland_property_changed(
        &mut self,
        state: &ServerState,
        host_xid: u32,
        property: yserver_protocol::x11::AtomId,
    ) {
        if let Some(link) = self.pstate.wayland.as_mut() {
            link.property_changed(state, host_xid, property);
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
        yserver_core::backend::Backend::get_active_cursor_image(self)
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

    fn read_image(
        &mut self,
        host_xid: u32,
        width: u16,
        height: u16,
        border: u16,
    ) -> Option<Vec<u8>> {
        let target = self.resolve_paint_target(host_xid)?;
        let depth = target.x11_depth();
        if depth != 24 && depth != 32 {
            return None;
        }
        // The content, and `border` pixels of the border around it:
        // `get_image`'s rectangle at the window's own origin, less the
        // border, which the backing holds around the content.
        let border = u32::from(border);
        let (width, height) = (
            u32::from(width) + 2 * border,
            u32::from(height) + 2 * border,
        );
        let inset = i32::try_from(border).unwrap_or(0);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: target.offset().0 - inset,
                y: target.offset().1 - inset,
            },
            extent: ash::vk::Extent2D { width, height },
        };
        let readback = self.engine.get_image(
            &mut self.store,
            self.platform.gpu_mut(),
            target.src_including_border(),
            rect,
            depth,
        );
        self.drain_frame_builder_telemetry();
        match readback {
            Ok(mut bytes) => {
                // Short only while a resize is under way: pad, as
                // `get_image` does, so the rows stay where they belong.
                bytes.resize(width as usize * height as usize * 4, 0);
                Some(bytes)
            }
            Err(error) => {
                log::warn!("wayland: reading window 0x{host_xid:x} back: {error:?}");
                None
            }
        }
    }
}
