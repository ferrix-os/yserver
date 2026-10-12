//! `impl Backend for RootlessBackend`: every method the rootless backend
//! does not take for itself (`own_*` in `super`) goes to the renderer
//! it wraps, unchanged. Mechanical on purpose, like `host_x11`'s
//! `trait_impl.rs`: a trait change lands by editing this file, and
//! `delegate_tests` fails when a method of the trait is neither here nor
//! overridden, which a default method would otherwise hide.

#![allow(unused_imports, clippy::too_many_arguments)]

use std::{any::Any, io};

use yserver_core::{
    backend::*,
    core_loop::HostInputEvent,
    host_x11::{HostEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
    server::ServerState,
};
use yserver_protocol::x11::{AtomId, ClipRectangles, FontMetrics, ResourceId, glx, xfixes};

use super::{RootlessBackend, RootlessHost};

impl<H: RootlessHost + 'static> Backend for RootlessBackend<H> {
    fn window_id(&self) -> u32 {
        self.inner.window_id()
    }

    fn root_visual_xid(&self) -> u32 {
        self.inner.root_visual_xid()
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        self.inner.argb_visual_xid()
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        self.inner.argb_colormap_xid()
    }

    fn fb_dimensions(&self) -> (u16, u16) {
        self.inner.fb_dimensions()
    }

    fn randr_outputs_and_modes(
        &mut self,
    ) -> (
        Vec<yserver_core::randr::RandrOutput>,
        Vec<yserver_core::randr::RandrMode>,
    ) {
        self.inner.randr_outputs_and_modes()
    }

    fn randr_providers(&mut self) -> Vec<yserver_core::randr::RandrProvider> {
        self.inner.randr_providers()
    }

    fn render_opcode(&self) -> Option<u8> {
        self.inner.render_opcode()
    }

    fn xkb_opcode(&self) -> Option<u8> {
        self.inner.xkb_opcode()
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        self.inner.xkb_info()
    }

    fn set_locked_group(&mut self, group: u8) {
        self.inner.set_locked_group(group)
    }

    fn current_group(&self) -> u8 {
        self.inner.current_group()
    }

    fn current_xkb_mods(&self) -> (u8, u8, u8, u8) {
        self.inner.current_xkb_mods()
    }

    fn composite_opcode(&self) -> Option<u8> {
        self.inner.composite_opcode()
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        self.inner.render_format_for_ynest_id(ynest_fmt)
    }

    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.inner.ping(origin)
    }

    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent) {
        self.inner.on_host_input(state, ev)
    }

    fn sync_floating_keyboard_states(&mut self, state: &ServerState) {
        self.inner.sync_floating_keyboard_states(state)
    }

    fn disable_xi_facet(&mut self, state: &mut ServerState, device_id: u16) {
        self.inner.disable_xi_facet(state, device_id)
    }

    fn enable_xi_facet(&mut self, state: &mut ServerState, device_id: u16) {
        self.inner.enable_xi_facet(state, device_id)
    }

    fn on_page_flip_ready(&mut self, state: &mut ServerState, drm_fd: std::os::fd::RawFd) {
        self.inner.on_page_flip_ready(state, drm_fd)
    }

    fn on_display_hotplug(&mut self, state: &mut ServerState) {
        self.inner.on_display_hotplug(state)
    }

    fn on_scanout_render_completion(&mut self, state: &mut ServerState) {
        self.inner.on_scanout_render_completion(state)
    }

    fn reprobe_connectors(&mut self, state: &mut ServerState) -> io::Result<()> {
        self.inner.reprobe_connectors(state)
    }

    fn set_provider_output_source(
        &mut self,
        state: &mut ServerState,
        provider: u32,
        source_provider: Option<u32>,
    ) -> io::Result<bool> {
        self.inner
            .set_provider_output_source(state, provider, source_provider)
    }

    fn apply_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<bool> {
        self.inner
            .apply_crtc_config(output_id, connector, mode, x, y)
    }

    fn begin_crtc_config(
        &mut self,
        output_id: u32,
        connector: &str,
        mode: Option<ModeSpec>,
        x: i32,
        y: i32,
    ) -> io::Result<CrtcConfigApply> {
        self.inner
            .begin_crtc_config(output_id, connector, mode, x, y)
    }

    fn drain_ready_crtc_configs(&mut self) -> Vec<CrtcConfigToken> {
        self.inner.drain_ready_crtc_configs()
    }

    fn finish_crtc_config(&mut self, token: CrtcConfigToken) -> io::Result<bool> {
        self.inner.finish_crtc_config(token)
    }

    fn cancel_crtc_config(&mut self, token: CrtcConfigToken) {
        self.inner.cancel_crtc_config(token)
    }

    fn crtc_gamma_size(&self, crtc: u32) -> u16 {
        self.inner.crtc_gamma_size(crtc)
    }

    fn set_crtc_gamma(
        &mut self,
        crtc: u32,
        red: &[u16],
        green: &[u16],
        blue: &[u16],
    ) -> io::Result<()> {
        self.inner.set_crtc_gamma(crtc, red, green, blue)
    }

    fn get_crtc_gamma(&self, crtc: u32) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        self.inner.get_crtc_gamma(crtc)
    }

    fn refresh_randr_state_set_time(&mut self, state: &mut ServerState, set_time: u32) {
        self.inner.refresh_randr_state_set_time(state, set_time)
    }

    fn randr_layout_changed(&mut self, state: &mut ServerState) {
        self.inner.randr_layout_changed(state)
    }

    fn set_logical_screen_size(&mut self, w: u16, h: u16) -> io::Result<()> {
        self.inner.set_logical_screen_size(w, h)
    }

    fn vt_switching_armed(&self) -> bool {
        self.inner.vt_switching_armed()
    }

    fn on_vt_release(&mut self, state: &mut ServerState) {
        self.inner.on_vt_release(state)
    }

    fn begin_vt_release(&mut self) -> bool {
        self.inner.begin_vt_release()
    }

    fn finish_vt_release(
        &mut self,
        state: &mut ServerState,
        input_inventory: &yserver_core::core_loop::input_inventory::InputInventory,
    ) {
        self.inner.finish_vt_release(state, input_inventory)
    }

    fn on_vt_acquire(&mut self, state: &mut ServerState) {
        self.inner.on_vt_acquire(state)
    }

    fn request_vt_switch(&mut self, vt: u32) {
        self.inner.request_vt_switch(vt)
    }

    fn on_libinput_ready(&mut self, state: &mut ServerState) {
        self.inner.on_libinput_ready(state)
    }

    fn on_host_display_ready(&mut self, state: &mut ServerState) -> bool {
        self.own_on_host_display_ready(state)
    }

    fn poll_deferred_input(&mut self, state: &mut ServerState) {
        self.own_poll_deferred_input(state)
    }

    fn before_block(&mut self) {
        self.own_before_block()
    }

    fn probe_input_devices(&mut self, state: &mut ServerState) -> usize {
        self.inner.probe_input_devices(state)
    }

    fn start_device_config(
        &mut self,
        source: yserver_core::xinput::InputSourceId,
        change: yserver_core::xinput::libinput_props::DeviceConfigChange,
        cancel: yserver_core::xinput::libinput_props::DeviceConfigCancelToken,
    ) -> Result<
        yserver_core::xinput::libinput_props::DeviceConfigStart,
        yserver_core::xinput::libinput_props::DeviceConfigError,
    > {
        self.inner.start_device_config(source, change, cancel)
    }

    fn set_input_sender(&mut self, sender: yserver_core::core_loop::CoreSender) {
        self.inner.set_input_sender(sender)
    }

    fn mark_dirty(&mut self) {
        self.inner.mark_dirty()
    }

    fn flush_before_damage_notify(&mut self) {
        self.inner.flush_before_damage_notify()
    }

    fn next_wakeup(&self) -> Option<std::time::Instant> {
        self.own_next_wakeup()
    }

    fn maybe_composite(&mut self) -> io::Result<()> {
        self.inner.maybe_composite()
    }

    fn dump_scanout(&mut self) {
        self.inner.dump_scanout()
    }

    fn dump_drawables(&mut self) {
        self.inner.dump_drawables()
    }

    fn report_export_holders(
        &mut self,
        core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        self.inner.report_export_holders(core)
    }

    fn on_window_property_changed(&mut self, state: &ServerState, host_xid: u32, property: AtomId) {
        self.own_on_window_property_changed(state, host_xid, property)
    }

    fn set_keymap_rmlvo(
        &mut self,
        rules: &str,
        model: &str,
        layout: &str,
        variant: &str,
        options: Option<&str>,
    ) -> Option<(u8, u8)> {
        self.inner
            .set_keymap_rmlvo(rules, model, layout, variant, options)
    }

    fn current_xkb_rules_names(&self) -> Option<[String; 5]> {
        self.inner.current_xkb_rules_names()
    }

    fn on_window_became_top_level(&mut self, state: &ServerState, host_xid: u32) {
        self.inner.on_window_became_top_level(state, host_xid)
    }

    fn sync_top_level_order(&mut self, state: &ServerState) {
        self.inner.sync_top_level_order(state)
    }

    fn note_present_pixmap(&mut self, src_pixmap_xid: u32, dst_window_xid: u32) {
        self.inner
            .note_present_pixmap(src_pixmap_xid, dst_window_xid)
    }

    fn note_present_scanout_candidate(&mut self, candidate: PresentScanoutCandidate) {
        self.inner.note_present_scanout_candidate(candidate)
    }

    fn try_present_direct(
        &mut self,
        candidate: PresentScanoutCandidate,
        event: CompletedPresentEvent,
    ) -> std::io::Result<bool> {
        self.inner.try_present_direct(candidate, event)
    }

    fn note_present_skip(&mut self) {
        self.inner.note_present_skip()
    }

    fn arm_present_source_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
    ) -> std::io::Result<PresentSourceWait> {
        self.inner
            .arm_present_source_wait(src_pixmap_host_xid, dst_window_host_xid)
    }

    fn arm_present_syncobj_wait(
        &mut self,
        src_pixmap_host_xid: u32,
        dst_window_host_xid: u32,
        acquire_syncobj: u32,
        acquire_value: u64,
    ) -> std::io::Result<PresentSourceWait> {
        self.inner.arm_present_syncobj_wait(
            src_pixmap_host_xid,
            dst_window_host_xid,
            acquire_syncobj,
            acquire_value,
        )
    }

    fn drain_ready_present_source_waits(&mut self) -> Vec<u64> {
        self.inner.drain_ready_present_source_waits()
    }

    fn begin_ready_present_destination_write(&mut self, wait_id: u64) {
        self.inner.begin_ready_present_destination_write(wait_id)
    }

    fn finish_present_source_wait(&mut self, wait_id: u64) {
        self.inner.finish_present_source_wait(wait_id)
    }

    fn present_flip_in_flight(&self, crtc_id: u32) -> bool {
        self.inner.present_flip_in_flight(crtc_id)
    }

    fn present_display_idle(&self, crtc_id: u32) -> bool {
        self.inner.present_display_idle(crtc_id)
    }

    fn present_crtc_clock_epoch(&self, crtc_id: u32) -> u64 {
        self.inner.present_crtc_clock_epoch(crtc_id)
    }

    fn present_absolute_vblank_arm_supported(&self, crtc_id: u32) -> bool {
        self.inner.present_absolute_vblank_arm_supported(crtc_id)
    }

    fn arm_present_absolute_vblank(
        &mut self,
        crtc_id: u32,
        targets: &[u64],
    ) -> std::io::Result<usize> {
        self.inner.arm_present_absolute_vblank(crtc_id, targets)
    }

    fn present_scanout_blackout(&self) -> bool {
        self.inner.present_scanout_blackout()
    }

    fn pin_present_source(&mut self, host_xid: u32) -> Option<u64> {
        self.inner.pin_present_source(host_xid)
    }

    fn release_present_source(&mut self, pin_id: u64) {
        self.inner.release_present_source(pin_id)
    }

    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)> {
        self.own_poll_fds()
    }

    fn as_any(&self) -> &dyn Any {
        self.inner.as_any()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        self.inner.create_subwindow(
            origin,
            host_parent,
            x,
            y,
            width,
            height,
            border_width,
            visual,
            background_pixel,
            background_pixmap,
        )
    }

    fn destroy_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.inner.destroy_subwindow(origin, host_xid)
    }

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.inner.map_subwindow(origin, host_xid)
    }

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.inner.unmap_subwindow(origin, host_xid)
    }

    fn realize_window_storage(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.inner.realize_window_storage(origin, host_xid)
    }

    fn release_window_storage(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.inner.release_window_storage(origin, host_xid)
    }

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        self.inner.configure_subwindow(origin, host_xid, config)
    }

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        self.inner
            .reparent_subwindow(origin, host_xid, host_parent, x, y)
    }

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        self.inner
            .change_subwindow_attributes(origin, host_xid, value_mask, values)
    }

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()> {
        self.inner
            .update_host_event_mask(origin, host_xid, mask, enabled)
    }

    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.inner.register_top_level(origin, nested_id, host_xid)
    }

    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.inner.register_subwindow(origin, nested_id, host_xid)
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        self.inner.unregister_host_window(host_xid)
    }

    fn xid_map(&self) -> &HostXidMap {
        self.inner.xid_map()
    }

    fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
        self.inner.drain_host_socket()
    }

    fn pop_pending_host_event(&mut self) -> Option<HostEvent> {
        self.inner.pop_pending_host_event()
    }

    fn host_socket_eof(&self) -> bool {
        self.inner.host_socket_eof()
    }

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        self.inner.name_window_pixmap(origin, host_window)
    }

    fn allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        self.inner
            .allocate_redirected_backing(origin, host_window, width, height, depth)
    }

    fn release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.inner.release_redirected_backing(origin, backing)
    }

    fn retain_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.inner.retain_backing_storage(origin, backing)
    }

    fn drop_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.inner.drop_backing_storage(origin, backing)
    }

    fn release_window_pixmap_name(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        self.inner.release_window_pixmap_name(origin, backing)
    }

    fn redirected_backing_can_fit(
        &self,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> bool {
        self.inner
            .redirected_backing_can_fit(backing, width, height, depth)
    }

    fn update_redirected_backing_geometry(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<()> {
        self.inner
            .update_redirected_backing_geometry(origin, backing, width, height, depth)
    }

    fn supports_redirect_activation(&self) -> bool {
        self.inner.supports_redirect_activation()
    }

    fn supports_dmabuf_export(&self) -> bool {
        self.inner.supports_dmabuf_export()
    }

    fn glx_vendor_names(&self) -> &'static str {
        self.inner.glx_vendor_names()
    }

    fn dpms_capable(&self) -> bool {
        self.inner.dpms_capable()
    }

    fn set_dpms_power(&mut self, level: u8) -> std::io::Result<()> {
        self.inner.set_dpms_power(level)
    }

    fn set_window_scene_participation(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.inner
            .set_window_scene_participation(origin, host_window, participating)
    }

    fn set_backing_scene_participation(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        self.inner
            .set_backing_scene_participation(origin, backing, participating)
    }

    fn get_overlay_window(&mut self, origin: Option<OriginContext>) -> io::Result<bool> {
        self.inner.get_overlay_window(origin)
    }

    fn release_overlay_window(&mut self, origin: Option<OriginContext>) -> io::Result<bool> {
        self.inner.release_overlay_window(origin)
    }

    fn cow_host_xid(&self) -> Option<u32> {
        self.inner.cow_host_xid()
    }

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        self.inner.create_pixmap(origin, depth, width, height)
    }

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.inner.free_pixmap(origin, host_xid)
    }

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        self.inner.open_font(origin, name)
    }

    fn set_font_path(
        &mut self,
        origin: Option<OriginContext>,
        paths: &[String],
    ) -> Result<(), usize> {
        self.inner.set_font_path(origin, paths)
    }

    fn font_path(&self) -> Vec<String> {
        self.inner.font_path()
    }

    fn paint_window_background_rect(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.inner
            .paint_window_background_rect(origin, host_xid, x, y, width, height)
    }

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.inner.close_font(origin, host_xid)
    }

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        self.inner
            .create_cursor(origin, source_pixmap, mask_pixmap, fore, back, hot_x, hot_y)
    }

    fn free_cursor(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.inner.free_cursor(origin, host_xid)
    }

    fn recolor_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<()> {
        self.inner.recolor_cursor(origin, host_xid, fore, back)
    }

    fn create_anim_cursor(
        &mut self,
        origin: Option<OriginContext>,
        frames: &[(CursorHandle, u32)],
    ) -> io::Result<Option<CursorHandle>> {
        self.inner.create_anim_cursor(origin, frames)
    }

    fn create_glyph_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        self.inner.create_glyph_cursor(
            origin,
            source_font,
            mask_font,
            source_char,
            mask_char,
            fore,
            back,
        )
    }

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        self.inner
            .define_cursor(origin, host_window_xid, cursor_host_xid)
    }

    fn set_grab_cursor(
        &mut self,
        origin: Option<OriginContext>,
        cursor_host_xid: Option<u32>,
    ) -> io::Result<()> {
        self.inner.set_grab_cursor(origin, cursor_host_xid)
    }

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.inner.set_container_background_pixel(origin, pixel)
    }

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.inner
            .set_container_background_pixmap(origin, host_pixmap_xid)
    }

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.inner.clear_clip_rectangles(origin)
    }

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.inner.set_clip_rectangles(origin, clip)
    }

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        self.inner
            .set_clip_pixmap(origin, host_pixmap, clip_x_origin, clip_y_origin)
    }

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.inner.set_gc_fill_solid(origin)
    }

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        self.inner
            .set_gc_fill_tiled(origin, host_pixmap, tile_x_origin, tile_y_origin)
    }

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        self.inner.apply_clip_state(origin, clip)
    }

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.inner.apply_fill_state(origin, fill)
    }

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        self.inner.apply_draw_state(origin, state)
    }

    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.inner.copy_area(
            origin,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        )
    }

    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        self.inner.copy_plane(
            origin,
            src_host_xid,
            dst_host_xid,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
            plane,
        )
    }

    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        self.inner
            .put_image(origin, host_xid, depth, width, height, dst_x, dst_y, data)
    }

    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        self.inner
            .get_image(origin, host_xid, format, x, y, width, height, plane_mask)
    }

    fn clear_area(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        self.inner.clear_area(
            origin,
            host_xid,
            background_pixel,
            background_pixmap_host_xid,
            x,
            y,
            width,
            height,
            tile_origin,
        )
    }

    fn read_depth1_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        self.inner.read_depth1_pixmap(origin, host_xid)
    }

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.inner
            .poly_line(origin, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        self.inner
            .poly_segment(origin, host_xid, foreground, segments)
    }

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.inner
            .poly_rectangle(origin, host_xid, foreground, rectangles)
    }

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        self.inner.poly_arc(origin, host_xid, foreground, arcs)
    }

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.inner
            .poly_point(origin, host_xid, foreground, coordinate_mode, points)
    }

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.inner
            .poly_fill_rectangle(origin, host_xid, foreground, rectangles)
    }

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        self.inner.poly_fill_arc(origin, host_xid, foreground, arcs)
    }

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.inner
            .fill_poly(origin, host_xid, foreground, coord_mode, points)
    }

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.inner
            .fill_rectangle(origin, host_xid, foreground, x, y, width, height)
    }

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner.poly_text8(origin, host_xid, foreground, body)
    }

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner.poly_text16(origin, host_xid, foreground, body)
    }

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner
            .image_text8(origin, host_xid, foreground, background, text_len, body)
    }

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner
            .image_text16(origin, host_xid, foreground, background, text_len, body)
    }

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.inner
            .render_create_picture(origin, host_drawable, ynest_format, value_mask, values)
    }

    fn set_picture_drawable_origin(&mut self, host_pic: u32, origin: (i16, i16)) {
        self.inner.set_picture_drawable_origin(host_pic, origin)
    }

    fn picture_client_clip_rects(
        &mut self,
        host_pic: u32,
    ) -> Option<Option<Vec<xfixes::RegionRect>>> {
        self.inner.picture_client_clip_rects(host_pic)
    }

    fn picture_includes_inferiors(&self, host_pic: u32) -> bool {
        self.inner.picture_includes_inferiors(host_pic)
    }

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner.render_change_picture(origin, host_pic, body)
    }

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        self.inner.render_free_picture(origin, host_pic)
    }

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        self.inner.render_create_glyphset(origin, ynest_format)
    }

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        self.inner.render_free_glyphset(origin, host_gs)
    }

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        self.inner.render_add_glyphs(origin, host_gs, body_tail)
    }

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        self.inner.render_free_glyphs(origin, host_gs, glyph_ids)
    }

    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        self.inner.render_composite(
            origin, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x, dst_y,
            width, height,
        )
    }

    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        self.inner.render_composite_glyphs(
            origin, minor, op, host_src, host_dst, mask_fmt, host_gs, src_x, src_y, items, x_off,
            y_off,
        )
    }

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        self.inner
            .render_fill_rectangles(origin, host_dst, op, color, rects, x_off, y_off)
    }

    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        self.inner.render_trapezoids(
            origin,
            op,
            host_src,
            host_dst,
            host_mask_format,
            src_x,
            src_y,
            traps,
            x_off,
            y_off,
        )
    }

    fn render_triangles_op(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<Vec<xfixes::RegionRect>> {
        self.inner.render_triangles_op(
            origin,
            minor,
            op,
            host_src,
            host_dst,
            host_mask_format,
            src_x,
            src_y,
            primitives,
            x_off,
            y_off,
        )
    }

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        self.inner.render_create_solid_fill(origin, color)
    }

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.inner.render_create_linear_gradient(origin, body)
    }

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.inner.render_create_radial_gradient(origin, body)
    }

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        self.inner.render_create_cursor(origin, host_src_pic, x, y)
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner
            .render_set_picture_clip_rectangles(origin, host_pic, body)
    }

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner.render_set_picture_filter(origin, host_pic, body)
    }

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.inner
            .render_set_picture_transform(origin, host_pic, body)
    }

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        self.inner.render_query_version(origin)
    }

    fn dri3_open(&mut self, drawable: u32) -> io::Result<std::os::fd::OwnedFd> {
        self.inner.dri3_open(drawable)
    }

    fn dri3_capabilities(&self) -> Dri3Caps {
        self.inner.dri3_capabilities()
    }

    fn dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: Dri3ImportModifier,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        self.inner
            .dri3_import_pixmap(fd, width, height, stride, offset, modifier, depth, bpp)
    }

    fn dri3_supported_modifiers(&self, window: u32, depth: u8, bpp: u8) -> (Vec<u64>, Vec<u64>) {
        self.inner.dri3_supported_modifiers(window, depth, bpp)
    }

    fn dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        self.inner.dri3_export_pixmap(host_xid)
    }

    fn dri3_export_pixmap_buffers(&mut self, host_xid: u32) -> io::Result<Dri3PixmapExport> {
        self.inner.dri3_export_pixmap_buffers(host_xid)
    }

    fn dri3_fence_from_fd(&mut self, fence_xid: u32, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        self.inner.dri3_fence_from_fd(fence_xid, fd)
    }

    fn dri3_fd_from_fence(&mut self, fence_xid: u32) -> io::Result<std::os::fd::OwnedFd> {
        self.inner.dri3_fd_from_fence(fence_xid)
    }

    fn dri3_import_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        self.inner.dri3_import_syncobj(client_id, syncobj_xid, fd)
    }

    fn dri3_free_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> io::Result<()> {
        self.inner.dri3_free_syncobj(client_id, syncobj_xid)
    }

    fn dri3_signal_syncobj(&mut self, syncobj_xid: u32, value: u64) -> io::Result<()> {
        self.inner.dri3_signal_syncobj(syncobj_xid, value)
    }

    fn dri3_syncobj_handle(&self, syncobj_xid: u32) -> Option<std::sync::Arc<dyn SyncobjHandle>> {
        self.inner.dri3_syncobj_handle(syncobj_xid)
    }

    fn dri3_syncobj_owned(
        &self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        self.inner.dri3_syncobj_owned(client_id, syncobj_xid)
    }

    fn dri3_signal_syncobj_via_handle(
        &mut self,
        handle: &std::sync::Arc<dyn SyncobjHandle>,
        value: u64,
    ) -> std::io::Result<()> {
        self.inner.dri3_signal_syncobj_via_handle(handle, value)
    }

    fn dri3_trigger_fence(&mut self, fence_xid: u32) -> io::Result<()> {
        self.inner.dri3_trigger_fence(fence_xid)
    }

    fn dri3_fence_triggered(&self, fence_xid: u32) -> Option<bool> {
        self.inner.dri3_fence_triggered(fence_xid)
    }

    fn dri3_reset_fence(&mut self, fence_xid: u32) {
        self.inner.dri3_reset_fence(fence_xid)
    }

    fn dri3_destroy_fence(&mut self, fence_xid: u32) {
        self.inner.dri3_destroy_fence(fence_xid)
    }

    fn dri3_xshmfence_handle(&self, fence_xid: u32) -> Option<std::sync::Arc<dyn XshmfenceHandle>> {
        self.inner.dri3_xshmfence_handle(fence_xid)
    }

    fn dri3_trigger_fence_via_handle(
        &mut self,
        handle: &std::sync::Arc<dyn XshmfenceHandle>,
    ) -> std::io::Result<()> {
        self.inner.dri3_trigger_fence_via_handle(handle)
    }

    fn present_capabilities(&self, window: u32) -> PresentCaps {
        self.inner.present_capabilities(window)
    }

    fn enqueue_present_completion(&mut self, event: CompletedPresentEvent, dst_host_xid: u32) {
        self.inner.enqueue_present_completion(event, dst_host_xid)
    }

    fn drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent> {
        self.inner.drain_completed_present_events()
    }

    fn drain_retired_present_idle_events(&mut self) -> Vec<CompletedPresentEvent> {
        self.inner.drain_retired_present_idle_events()
    }

    fn signal_present_wake(&mut self, present_id: u64) {
        self.inner.signal_present_wake(present_id)
    }

    fn present_get_ust_msc(&self, crtc_id: u32) -> (u64, u64) {
        self.inner.present_get_ust_msc(crtc_id)
    }

    fn present_get_completion_clock(&self, crtc_id: u32) -> PresentClockSample {
        self.inner.present_get_completion_clock(crtc_id)
    }

    fn arm_idle_vblanks(&mut self, crtc_id: u32, target_mscs: &[u64]) -> std::io::Result<usize> {
        self.inner.arm_idle_vblanks(crtc_id, target_mscs)
    }

    fn arm_present_completion_idle_vblanks(
        &mut self,
        crtc_id: u32,
        target_mscs: &[u64],
    ) -> std::io::Result<usize> {
        self.inner
            .arm_present_completion_idle_vblanks(crtc_id, target_mscs)
    }

    fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        self.inner.acquire_glx_pixmap_export(host_xid)
    }

    fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        self.inner.release_glx_pixmap_export(host_xid)
    }

    fn client_disconnected(&mut self, client_id: yserver_protocol::x11::ClientId) {
        self.inner.client_disconnected(client_id)
    }

    fn reset_input_session(&mut self, old_state: &mut ServerState) {
        self.inner.reset_input_session(old_state)
    }

    fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        self.inner.promote_pixmap_exportable(host_xid)
    }

    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        self.inner.xkb_proxy(origin, minor, body, intern_atom)
    }

    fn xkb_set(
        &mut self,
        minor: u8,
        body: &[u8],
        client_is_ancient: bool,
        atom_name: &dyn Fn(u32) -> Option<String>,
    ) -> Option<XkbSetOutcome> {
        self.inner
            .xkb_set(minor, body, client_is_ancient, atom_name)
    }

    fn load_keymap_by_components(&mut self, symbols: &str) -> KeymapLoad {
        self.inner.load_keymap_by_components(symbols)
    }

    fn xkb_get_kbd_by_name(
        &mut self,
        body: &[u8],
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> Option<(Vec<u8>, Option<XkbNewKeyboardInfo>)> {
        self.inner.xkb_get_kbd_by_name(body, intern_atom)
    }

    fn replace_cursor(
        &mut self,
        origin: Option<OriginContext>,
        old_host_xid: u32,
        new_host_xid: u32,
    ) -> io::Result<()> {
        self.inner
            .replace_cursor(origin, old_host_xid, new_host_xid)
    }

    fn set_cursor_hidden(&mut self, hidden: bool) {
        self.inner.set_cursor_hidden(hidden)
    }

    fn take_displayed_cursor_change(&mut self) -> Option<DisplayedCursor> {
        self.inner.take_displayed_cursor_change()
    }

    fn get_active_cursor_image(&self) -> Option<ActiveCursorImage> {
        self.inner.get_active_cursor_image()
    }

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: Option<&[xfixes::RegionRect]>,
    ) -> io::Result<()> {
        self.inner
            .set_shape_rectangles(origin, host_xid, kind, rects)
    }

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()> {
        self.inner.warp_pointer(origin, dst_host_xid, dst_x, dst_y)
    }

    fn warp_pointer_root(&mut self, state: &mut ServerState, x: i32, y: i32) {
        self.inner.warp_pointer_root(state, x, y)
    }

    fn windows_restructured(&mut self, state: &mut ServerState) {
        self.inner.windows_restructured(state)
    }

    fn resync_input_position(&mut self) {
        self.inner.resync_input_position()
    }

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        self.inner.query_pointer(origin)
    }

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        self.inner.list_fonts_proxy(origin, max_names, pattern)
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        self.inner
            .list_fonts_with_info_proxy(origin, max_names, pattern, intern_atom)
    }

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>> {
        self.inner.get_atom_name(origin, atom)
    }

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        self.inner
            .get_keyboard_mapping(origin, first_keycode, count)
    }

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)> {
        self.inner.get_modifier_mapping(origin)
    }

    fn change_keyboard_mapping(
        &mut self,
        first_keycode: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) -> Option<KeyboardMappingChange> {
        self.inner
            .change_keyboard_mapping(first_keycode, keysyms_per_keycode, keysyms)
    }

    fn set_modifier_mapping(&mut self, modmap: &[u8; 256]) -> Option<KeyboardMappingChange> {
        self.inner.set_modifier_mapping(modmap)
    }

    fn keymap_auto_repeats(&self) -> Option<[u8; 32]> {
        self.inner.keymap_auto_repeats()
    }

    fn output_identity(&self, output_id: u32) -> Option<(Vec<u8>, String)> {
        self.inner.output_identity(output_id)
    }
}
