//! What a native host implements and receives (spec §4.5).

use std::io;

use ash::vk;
use yserver_core::{
    backend::ActiveCursorImage,
    fd::OwnedFd,
    rootless::{Rect, RootlessEvent, ToplevelId},
};

use crate::kms::vk::device::VkContext;

/// Per top-level, monotonic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct FrameSeq(pub u64);

/// How a host takes a top-level's pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HandoffKind {
    /// A Vulkan surface on the same device; the presenter owns its swapchain.
    VulkanSurface,
    /// A dmabuf of the backing (Linux). RL1 answers it with `Cpu` (D4).
    Dmabuf,
    /// Bytes.
    Cpu,
}

/// An owned CPU frame; every byte equals the canonical image at `seq`
/// (spec §4.5.2).
#[derive(Debug)]
pub(crate) struct CpuFrame {
    pub id: ToplevelId,
    pub seq: FrameSeq,
    /// The backing allocation the bytes came from.
    pub backing_gen: u64,
    /// Backing extent = native content (spec §4.0).
    pub width: u16,
    pub height: u16,
    pub stride: u32,
    /// 24: alpha undefined; 32: premultiplied ARGB.
    pub depth: u8,
    /// B, G, R, A rows, full extent.
    pub pixels: Vec<u8>,
    /// Backing pixels changed since the last delivered frame.
    pub damage: Vec<Rect>,
}

#[allow(dead_code, reason = "the dmabuf handoff is RL3 (D4)")]
#[derive(Debug)]
pub(crate) struct DmabufFrame {
    pub id: ToplevelId,
    pub seq: FrameSeq,
    pub backing_gen: u64,
    pub fds: Vec<OwnedFd>,
    pub stride: u32,
    pub modifier: u64,
    pub format: u32,
    pub damage: Vec<Rect>,
}

/// A client's LINEAR DRI3 buffer a host is to make its own object of, for
/// direct frames (`kms/render/backend/kms/direct_frames.rs`): handed over
/// once, before its first frame, with the client's descriptor.
#[cfg(kms)]
#[derive(Debug)]
pub(crate) struct DirectBuffer {
    /// Unique for the server's life: what the host keeps its object by.
    pub generation: u64,
    pub width: u16,
    pub height: u16,
    /// The one plane's offset and stride, in bytes, checked against the
    /// dma-buf's size and to fit 32 bits; its layout is
    /// `DRM_FORMAT_MOD_LINEAR`.
    pub offset: u32,
    pub stride: u32,
    /// 24: alpha undefined; 32: premultiplied ARGB.
    pub depth: u8,
    /// The buffer, a dma-buf.
    pub fd: OwnedFd,
}

/// A whole-window frame that is a client's own buffer, one the host was
/// given as a [`DirectBuffer`]: the client presented a buffer of the
/// window's size, and the window's backing is owed the frame instead of a
/// copy of it.
#[cfg(kms)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirectFrame {
    pub id: ToplevelId,
    pub generation: u64,
    pub width: u16,
    pub height: u16,
}

/// Host → presenter, over the platform link (spec §4.5, §4.5.1).
#[derive(Debug)]
pub(crate) enum FrameEvent {
    /// The setup handshake is done: ready for the first frame.
    SetupDone { id: ToplevelId },
    /// The native window was made again (a popup moved to another parent,
    /// or repositioned where the window system cannot move one): back to
    /// the setup handshake, nothing shown, and the next frame whole.
    #[cfg_attr(
        not(wayland_host),
        allow(dead_code, reason = "the Wayland host makes popups again")
    )]
    Recreated { id: ToplevelId },
    /// Can take another frame.
    Ready { id: ToplevelId },
    /// Shown / committed.
    Delivered { id: ToplevelId, seq: FrameSeq },
    /// Could not be shown.
    Dropped { id: ToplevelId, seq: FrameSeq },
    /// A `CpuFrame`'s bytes, back for reuse.
    Released {
        id: ToplevelId,
        seq: FrameSeq,
        pixels: Vec<u8>,
    },
    /// The window system gave back the client buffer of a direct frame
    /// (`wl_buffer.release`): it reads it no more. The platform's, not the
    /// presenter's.
    #[cfg(kms)]
    DirectReleased { generation: u64 },
}

/// A native window system's side of rootless mode (spec §4.5), driven on the
/// core thread by the presenter.
pub(crate) trait RootlessHost {
    /// Asked once at `Mapped`. `Dmabuf` is answered with `Cpu` until RL3 (D4).
    fn handoff(&self, id: ToplevelId, depth: u8) -> HandoffKind;
    /// `VulkanSurface`: called by the presenter on `SetupDone`.
    fn create_surface(&mut self, id: ToplevelId, vk: &VkContext) -> io::Result<vk::SurfaceKHR>;
    fn submit_cpu(&mut self, frame: CpuFrame);
    #[allow(dead_code, reason = "the dmabuf handoff is RL3 (D4)")]
    fn submit_dmabuf(&mut self, frame: DmabufFrame) {
        drop(frame);
    }
    /// The first frame was delivered (GPU: presented): make the window visible.
    fn first_frame(&mut self, id: ToplevelId);
    /// Lifecycle, after the presenter applied the batch.
    fn on_events(&mut self, events: &[RootlessEvent]);
    /// `VulkanSurface`: swapchain pixels per X pixel per axis, for a window
    /// system whose surfaces are in device pixels (Windows); 1 shows the
    /// backing 1:1.
    fn copy_scale(&self) -> u32 {
        1
    }
    /// `id` has a frame waiting for `Ready`: arm the pacing signal (D7).
    #[allow(dead_code, reason = "readiness gating (RL1 Task 5)")]
    fn frame_wanted(&mut self, _id: ToplevelId) {}
    /// Before a swapchain (re)build at `extent` backing pixels.
    fn surface_resizing(&mut self, _id: ToplevelId, _extent: (u32, u32)) {}
    /// After it, whatever the outcome: commit what the driver changed on
    /// the native surface.
    fn surface_rebuilt(&mut self, _id: ToplevelId) {}
    /// The presenter destroyed the last swapchain and the surface of `id`:
    /// the native window may be freed now.
    fn surface_released(&mut self, _id: ToplevelId) {}
    /// The cursor to show over the host's windows, when it changed; `None`
    /// shows none (the `None` cursor, or one hidden). Its pixels are
    /// straight-alpha BGRA. For a host with no shell of its own to tell.
    #[cfg_attr(not(kms), allow(dead_code))]
    fn set_cursor(&mut self, _cursor: Option<&ActiveCursorImage>) {}
    /// Whether the host takes direct frames of buffers of `depth`: its
    /// window system can show a client's dma-buf of that format as a
    /// window's buffer. Where no host does, no buffer is kept for them.
    #[cfg(kms)]
    fn takes_direct(&self, _depth: u8) -> bool {
        false
    }
    /// Make the host's object for a client's buffer, before its first
    /// frame; the descriptor is the host's from now on.
    #[cfg(kms)]
    fn import_direct(&mut self, buffer: DirectBuffer) {
        drop(buffer);
    }
    /// Show `frame` as its window's next frame: `true` when it was
    /// committed, after which the host owes a `Ready` as after a CPU frame,
    /// and a `DirectReleased` once the window system no longer reads the
    /// buffer. `false` declines it (no object of that buffer yet, one the
    /// window system refused, a window that cannot show it whole); the
    /// window is read back instead.
    #[cfg(kms)]
    fn submit_direct(&mut self, _frame: DirectFrame) -> bool {
        false
    }
    /// The client buffers of these generations went: the host's objects
    /// of them may go.
    #[cfg(kms)]
    fn forget_direct(&mut self, _generations: &[u64]) {}
}
