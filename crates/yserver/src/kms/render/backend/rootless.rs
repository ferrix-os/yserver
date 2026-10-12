//! What a rootless backend (`crate::rootless`) needs of `KmsBackend`, which
//! renders for it headless: each top-level's image read back, a RandR output
//! standing for the host's screen, and the host's keyboard layout.

use super::*;

/// The host screen a rootless backend shows the server's windows on, as
/// the one RandR output stands for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOutput {
    /// The output's name (`WAYLAND-1`, say).
    pub name: String,
    /// The screen's size in pixels, which the root takes.
    pub size: (u16, u16),
    /// How often the screen refreshes, in whole hertz.
    pub refresh_hz: u32,
}

impl KmsBackend {
    /// Stand the one RandR output for the host's screen, and give the root
    /// the screen's size. Called once, before the topology is read.
    ///
    /// # Errors
    ///
    /// Resizing the virtual screen failing.
    pub fn set_host_output(&mut self, output: HostOutput) -> io::Result<()> {
        let (width, height) = output.size;
        self.apply_virtual_screen_extent(width, height)?;
        self.host_output = Some(output);
        Ok(())
    }

    /// Take the host's keyboard layout and variant for the server's keymap,
    /// so a key means in X what it means in the host's other windows. Once,
    /// before any client connects: afterwards a new keymap would owe the
    /// clients a MappingNotify. Returns whether the keymap changed.
    pub fn adopt_keyboard_layout(&mut self, layout: &str, variant: &str) -> bool {
        let rmlvo = crate::kms::core::XkbRmlvo {
            layout: layout.to_owned(),
            variant: variant.to_owned(),
            ..self.core.xkb_rmlvo.clone()
        };
        self.core.recompile_keymap(&rmlvo).is_some()
    }

    /// The keyboard layout the server's keymap has.
    #[must_use]
    pub fn keyboard_layout(&self) -> &str {
        &self.core.xkb_rmlvo.layout
    }
}

/// Each top-level's image for a rootless backend: the window read through
/// its redirect routing, which under the server's own redirect of the root's
/// children is its backing, subwindows included.
impl yserver_core::rootless::RootlessRenderer for KmsBackend {
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
        // Storage at depth 24 and 32 is BGRA8, which is the host's buffer's
        // layout too: rows copied as they are. Short only while a resize is
        // under way, when what the backing lacks is black.
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
                log::warn!("rootless: reading window 0x{host_xid:x} back: {error:?}");
                false
            }
        }
    }
}
