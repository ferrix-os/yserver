//! Zero-copy presents for the rootless Wayland backend.
//!
//! A game's `PresentPixmap` of a whole-window DRI3 buffer would be copied
//! three times on its way to the compositor: the client's buffer into its
//! pixmap, the pixmap into the window's backing, and the backing read back
//! into a `wl_shm` buffer. Where the compositor takes dmabufs
//! (`zwp_linux_dmabuf_v1`), such a present copies nothing: the backing is
//! *owed* the pixmap ([`Owed`]), and the compositor is handed the client's
//! own buffer for the window (`crate::wayland`'s `show`). The backing is
//! settled -- the copy made after all -- the moment anything reads it or a
//! present that cannot go direct writes it (`KmsBackend::wl_direct_settle`).
//!
//! What it does not catch: a core drawing request other than `CopyArea`
//! into a window whose backing is owed draws over the frame before the last
//! direct one. The compositor keeps showing the client's buffer until the
//! next present, which settles or replaces it, so that is one frame of a
//! window drawn both ways.
//!
//! `YSERVER_WAYLAND_DIRECT=0` turns it off.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use super::store::DrawableId;

/// A DRI3 buffer's single LINEAR plane, as the client gave it: what a
/// `zwp_linux_buffer_params_v1` for it says.
pub(crate) struct Dri3Layout {
    /// The client's buffer, held for as long as its pixmap.
    pub(crate) fd: OwnedFd,
    pub(crate) offset: u32,
    pub(crate) stride: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
    /// Unique for the server's life, unlike the pixmap's XID: what the
    /// compositor's `wl_buffer` for it is kept by.
    pub(crate) generation: u64,
}

/// A window backing owed a present that was handed to the compositor
/// instead of copied.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owed {
    /// The presented pixmap's host XID.
    pub(crate) src: u32,
    /// The window it was presented to, its host XID.
    pub(crate) dst: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// Which direct present this is, counted over all windows.
    pub(crate) serial: u64,
}

/// The backend's side of zero-copy presents.
#[derive(Default)]
pub(crate) struct WlDirect {
    /// By pixmap host XID.
    pub(crate) layouts: HashMap<u32, Dri3Layout>,
    /// By the backing owed.
    pub(crate) owed: HashMap<DrawableId, Owed>,
    /// Generations of layouts whose pixmap went, for the link to destroy
    /// its buffers of.
    pub(crate) freed: Vec<u64>,
    next_generation: u64,
    next_serial: u64,
    enabled: Option<bool>,
}

impl WlDirect {
    /// Whether direct presents are on (`YSERVER_WAYLAND_DIRECT` is not
    /// `0`), read once.
    pub(crate) fn enabled(&mut self) -> bool {
        *self.enabled.get_or_insert_with(|| {
            let on = std::env::var("YSERVER_WAYLAND_DIRECT").map_or(true, |value| value != "0");
            log::info!(
                "wayland: zero-copy presents {}",
                if on {
                    "on"
                } else {
                    "off (YSERVER_WAYLAND_DIRECT=0)"
                }
            );
            on
        })
    }

    /// Keep a LINEAR DRI3 buffer's layout for `pixmap`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record(
        &mut self,
        pixmap: u32,
        fd: OwnedFd,
        offset: u32,
        stride: u32,
        width: u16,
        height: u16,
        depth: u8,
    ) {
        if depth != 24 && depth != 32 {
            return;
        }
        self.next_generation += 1;
        let layout = Dri3Layout {
            fd,
            offset,
            stride,
            width,
            height,
            depth,
            generation: self.next_generation,
        };
        if let Some(old) = self.layouts.insert(pixmap, layout) {
            self.freed.push(old.generation);
        }
    }

    /// Forget `pixmap`'s layout; its owed presents must be settled first.
    pub(crate) fn forget(&mut self, pixmap: u32) {
        if let Some(old) = self.layouts.remove(&pixmap) {
            self.freed.push(old.generation);
        }
    }

    /// Owe `backing` a whole-window present of `src` to `dst`.
    pub(crate) fn owe(&mut self, backing: DrawableId, src: u32, dst: u32, width: u16, height: u16) {
        self.next_serial += 1;
        let _ = self.owed.insert(
            backing,
            Owed {
                src,
                dst,
                width,
                height,
                serial: self.next_serial,
            },
        );
    }

    /// The backings owed a present of `pixmap`.
    pub(crate) fn owed_from(&self, pixmap: u32) -> Vec<DrawableId> {
        self.owed
            .iter()
            .filter(|(_, owed)| owed.src == pixmap)
            .map(|(backing, _)| *backing)
            .collect()
    }
}
