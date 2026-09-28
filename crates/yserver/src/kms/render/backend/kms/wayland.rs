//! The rootless Wayland backend's half of `KmsBackend` (`crate::wayland`):
//! the compositor connection on the backend state, and the RandR topology
//! it implies.

use std::io;

use crate::kms::render::backend::KmsBackend;

impl KmsBackend {
    /// Become a client of a Wayland compositor (`crate::wayland`): take its
    /// first screen's size as the virtual screen's, and keep the connection
    /// for the core loop to poll. Called once, before the topology is read.
    ///
    /// # Errors
    ///
    /// Resizing the virtual screen failing.
    pub fn attach_wayland(&mut self, link: crate::wayland::WaylandLink) -> io::Result<()> {
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
        _state: &mut yserver_core::server::ServerState,
    ) -> bool {
        let Some(link) = self.pstate.wayland.as_mut() else {
            return true;
        };
        match link.dispatch() {
            Ok(events) => {
                for event in events {
                    log::debug!("wayland: {event:?}");
                }
                link.flush();
                true
            }
            Err(error) => {
                log::warn!("wayland: {error}");
                false
            }
        }
    }
}
