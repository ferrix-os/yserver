//! Direct frames: a whole-window `PresentPixmap` of a client's LINEAR DRI3
//! buffer handed to the rootless host as the client's buffer, instead of
//! copied into the window's backing and read back.
//!
//! Only where the host takes them (`RootlessHost::takes_direct`, for the
//! buffer's depth): where none does, no buffer is kept and no frame owed,
//! and every present is copied as before.
//!
//! # The backing is owed the frame
//!
//! A present that qualifies ([`KmsBackend::try_direct_frame`]: the buffer
//! is the window's size, at its origin, the whole of it, implicitly synced,
//! to a window that alone covers one of the presenter's top-levels) is not
//! copied. That top-level's backing is *owed* it ([`Owed`]), and the
//! presenter's tick hands the client's buffer to the host for the window
//! (`RootlessHost::submit_direct`) when the window is ready for its next
//! frame.
//!
//! The backing stays what X reads and draws, so an owed frame is copied in
//! -- *settled* -- before anything else reads or draws it, in request
//! order (`Platform::settle_for_access`):
//!
//! * where a draw finds its destination (`XBackend::resolve_paint_target`)
//!   or a read its source (`XBackend::resolve_paint_source`, RENDER
//!   pictures in `resolve_picture_for_render`): core drawing, `PutImage`,
//!   text, `CopyArea` and `CopyPlane` either side, RENDER either side,
//!   `GetImage`, a window's background and border, the copy that carries a
//!   resized window's content to its new backing, a child moved inside it;
//! * each inferior an `IncludeInferiors` stroke draws into, or a read
//!   pastes or snapshots, found by a walk that only looks;
//! * a pixmap read by its XID (`XBackend::settle_drawable`): a tile, a
//!   window background or border pixmap, a RENDER cursor's source, the old
//!   backing a resize carries gravity children out of. A window's named
//!   pixmap is its backing, so any of these may be one. (Clip masks and
//!   depth-1 cursor sources are depth 1 or 8 and cannot be.)
//! * a draw into the presented buffer itself first settles every backing
//!   owed it; a read of it needs nothing;
//! * the leaves restored from a backing that is released, and a backing
//!   exported to a client's GPU (DRI3 or GLX), which takes no direct frames
//!   from then on;
//! * the presenter reads a window back only once it is settled: a window
//!   that can take a frame and is not handed this one ([`Action::Settle`]);
//! * a window the host never gets ready (hidden, say) is settled a second
//!   after the frame was owed ([`OWED_FOR`]), so its client gets the buffer
//!   back;
//! * a backing that goes is owed nothing any more, and a buffer that goes
//!   settles the backings owed it first.
//!
//! A frame the host declines is settled and read back, as before.
//!
//! # When a buffer is the client's again
//!
//! A present that was not copied completes as a copy's would, but its
//! `IdleNotify` (and idle fence) waits ([`DirectFrames`]'s `held`) until
//! nothing can read the buffer any more: no backing is owed it, the host
//! does not hold it (`FrameEvent::DirectReleased`, the window system's
//! `wl_buffer.release`), and the GPU copy that settled it, if one did,
//! has finished (or the renderer failed, or [`READ_WAIT_MAX`] passed). A
//! buffer the host holds is not handed to it again until it was given
//! back, so one release always answers one hand-over. A buffer presented
//! again before it went idle keeps both presents' idles until it is.
//!
//! This is our rule. Xwayland makes a flipped buffer the window's pixmap
//! itself (`present_set_tree_pixmap` in `xwl_present_execute`,
//! `hw/xwayland/xwayland-present.c`) and idles it at the compositor's
//! release (`xwl_present_buffer_release`); here the window keeps its
//! backing, which may still have to copy the buffer after the release.
//!
//! # What the client's buffer must be
//!
//! A DRI3 buffer is kept for direct frames ([`DirectFrames::record`]) only
//! where the client named its layout LINEAR, at depth 24 or 32, and its
//! descriptor is a dma-buf at least as large as that layout says, with an
//! extent that fits 32 bits ([`check_buffer`]): the host hands it to a
//! window system that may end the connection for a buffer out of its
//! bounds. Its descriptor is kept only until the host made its object of
//! it, the first time a frame of it is handed over, and for at most
//! [`KEPT_MAX`] buffers at once.

use std::{
    collections::{HashMap, HashSet},
    os::fd::{AsFd, OwnedFd},
    time::{Duration, Instant},
};

use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate};

use crate::{
    kms::render::{
        backend::{KmsBackend, seam::Access},
        gpu::FenceTicket,
        store::DrawableId,
        target::{Dst, Src},
    },
    rootless::{
        DirectWindow, Presenter,
        frames::Readiness,
        host::{DirectBuffer, DirectFrame, RootlessHost},
    },
};

/// How long a frame may stay owed to a window that never gets ready for
/// it before it is settled, so its client gets the buffer back. Our
/// choice; a second, the length of the timer Xwayland runs for a window
/// whose frame callback is pending (`TIMER_LEN_FLIP`,
/// `xwl_present_reset_timer`), though that timer moves the window's MSC on
/// and returns no buffer.
pub(crate) const OWED_FOR: Duration = Duration::from_secs(1);

/// How long an idle waits for the GPU copy that read its buffer before it
/// goes anyway, so a copy whose completion is never seen polls no longer.
pub(crate) const READ_WAIT_MAX: Duration = Duration::from_secs(5);

/// The most client buffers whose descriptor is kept, from the import
/// until the host made its object: a client importing buffers it never
/// presents holds that many descriptors of the server's at most. A game's
/// swapchain is three or four.
pub(crate) const KEPT_MAX: usize = 64;

/// `PresentOptionCopy` (`presentproto`): the client asked for a copy.
const PRESENT_OPTION_COPY: u32 = 0x2;

/// `DMA_BUF_MAGIC` (`include/uapi/linux/magic.h`): what `fstatfs` says of
/// a dma-buf.
#[cfg(target_os = "linux")]
const DMA_BUF_MAGIC: nix::sys::statfs::FsType = nix::sys::statfs::FsType(0x444d_4142);

/// A DRI3 buffer's one plane, as the client described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) offset: u32,
    pub(crate) stride: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) depth: u8,
}

/// Why a client's buffer is not kept for direct frames.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum Refused {
    #[error("depth {0} (only 24 and 32 are a wl_buffer format)")]
    Depth(u8),
    #[error("an empty buffer")]
    Empty,
    #[error("a stride of {stride} for {width} pixels")]
    Stride { stride: u32, width: u16 },
    #[error("an extent of {0} bytes, beyond 32 bits")]
    Range(u64),
    #[error("not a dma-buf")]
    NotDmaBuf,
    #[error("its size cannot be read: {0}")]
    Unreadable(nix::errno::Errno),
    #[error("{size} bytes where its layout needs {needs}")]
    TooSmall { size: u64, needs: u64 },
    #[error("{KEPT_MAX} buffers kept already")]
    TooMany,
}

/// Whether `fd` can be handed to a window system as `layout`. What a
/// compositor answers `zwp_linux_buffer_params_v1.create` with a fatal
/// error for, an X client's numbers cannot reach past this: no empty
/// buffer (`invalid_dimensions`); one plane of a single-plane format
/// (`incomplete`); and for `out_of_bounds`, a stride of at least a row,
/// `offset + stride * height` within the dma-buf and within 32 bits (so
/// `offset + stride` is too). The format and its LINEAR modifier
/// (`invalid_format`) are the host's to check against what the compositor
/// named.
///
/// # Errors
///
/// What [`Refused`] names.
pub(crate) fn check_buffer(fd: impl AsFd, layout: Layout) -> Result<(), Refused> {
    if layout.depth != 24 && layout.depth != 32 {
        return Err(Refused::Depth(layout.depth));
    }
    if layout.width == 0 || layout.height == 0 {
        return Err(Refused::Empty);
    }
    if u64::from(layout.stride) < u64::from(layout.width) * 4 {
        return Err(Refused::Stride {
            stride: layout.stride,
            width: layout.width,
        });
    }
    let needs = u64::from(layout.offset) + u64::from(layout.stride) * u64::from(layout.height);
    if needs > u64::from(u32::MAX) {
        return Err(Refused::Range(needs));
    }
    if !is_dma_buf(&fd)? {
        return Err(Refused::NotDmaBuf);
    }
    let size = nix::sys::stat::fstat(fd.as_fd())
        .map_err(Refused::Unreadable)
        .map(|stat| u64::try_from(stat.st_size).unwrap_or(0))?;
    if needs > size {
        return Err(Refused::TooSmall { size, needs });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_dma_buf(fd: &impl AsFd) -> Result<bool, Refused> {
    let fs = nix::sys::statfs::fstatfs(fd.as_fd()).map_err(Refused::Unreadable)?;
    Ok(fs.filesystem_type() == DMA_BUF_MAGIC)
}

/// No host outside Linux takes direct frames.
#[cfg(not(target_os = "linux"))]
fn is_dma_buf(_fd: &impl AsFd) -> Result<bool, Refused> {
    Ok(false)
}

/// A client buffer a direct frame may hand over.
struct Buffer {
    /// The client's buffer, until the host is given it.
    fd: Option<OwnedFd>,
    layout: Layout,
    /// Unique for the server's life, unlike the pixmap's XID: what the
    /// host keeps its own buffer object for it by.
    generation: u64,
    /// Handed to the host and not given back.
    host_holds: bool,
    /// The last GPU copy that read it (a settle), until which it is not
    /// idle.
    read_by: Option<FenceTicket>,
}

/// A frame a top-level's backing is owed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owed {
    /// The presented pixmap's host XID.
    pub(crate) src: u32,
    /// The window presented to, its host XID.
    pub(crate) dst: u32,
    /// The presenter's top-level whose backing it is.
    pub(crate) top: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// When it was owed.
    pub(crate) since: Instant,
    /// Handed to the host, which shows it.
    pub(crate) handed: bool,
}

/// An idle notification whose buffer nothing reads, to go out once the GPU
/// copy that read it last has finished.
struct DueIdle {
    event: CompletedPresentEvent,
    after: Option<FenceTicket>,
    since: Instant,
}

/// The platform's side of direct frames.
#[derive(Default)]
pub(crate) struct DirectFrames {
    /// By the pixmap's host XID.
    buffers: HashMap<u32, Buffer>,
    /// The pixmap of each generation in `buffers`.
    pixmaps: HashMap<u64, u32>,
    /// How many of `buffers` still have their descriptor.
    kept: usize,
    /// By the backing owed.
    owed: HashMap<DrawableId, Owed>,
    /// How many backings are owed each pixmap's buffer.
    owed_srcs: HashMap<u32, usize>,
    /// The idle notifications of direct presents whose buffer something may
    /// still read, by the pixmap's host XID.
    held: HashMap<u32, Vec<CompletedPresentEvent>>,
    due: Vec<DueIdle>,
    /// Direct presents whose completion the core has not drained yet: an
    /// idle retired before it would find no wake to release (the
    /// completion retains it), and the client would wait for ever.
    uncompleted: HashSet<u64>,
    /// Generations of buffers that went, for the host to destroy its
    /// objects of.
    freed: Vec<u64>,
    next_generation: u64,
    /// The top-level each `(window presented to, backing)` covers alone,
    /// while the window tree is at the generation it was found at.
    covers: (u64, HashMap<(u32, DrawableId), Option<u32>>),
}

impl DirectFrames {
    /// Keep `pixmap`'s buffer, `layout`, for direct frames.
    ///
    /// # Errors
    ///
    /// The buffer is not one a window system can be handed
    /// ([`check_buffer`]), or [`KEPT_MAX`] are kept; nothing is kept.
    pub(crate) fn record(
        &mut self,
        pixmap: u32,
        fd: OwnedFd,
        layout: Layout,
    ) -> Result<(), Refused> {
        if self.kept >= KEPT_MAX {
            return Err(Refused::TooMany);
        }
        check_buffer(&fd, layout)?;
        debug_assert!(
            !self.buffers.contains_key(&pixmap),
            "pixmap 0x{pixmap:x} recorded twice"
        );
        self.forget(pixmap);
        self.next_generation += 1;
        let generation = self.next_generation;
        let _ = self.buffers.insert(
            pixmap,
            Buffer {
                fd: Some(fd),
                layout,
                generation,
                host_holds: false,
                read_by: None,
            },
        );
        let _ = self.pixmaps.insert(generation, pixmap);
        self.kept += 1;
        Ok(())
    }

    pub(crate) fn layout(&self, pixmap: u32) -> Option<Layout> {
        self.buffers.get(&pixmap).map(|buffer| buffer.layout)
    }

    /// How many buffers still have their descriptor.
    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.kept
    }

    pub(crate) fn owed(&self, backing: DrawableId) -> Option<Owed> {
        self.owed.get(&backing).copied()
    }

    pub(crate) fn is_owed_anything(&self) -> bool {
        !self.owed.is_empty()
    }

    fn is_owed(&self, backing: DrawableId) -> bool {
        self.owed.contains_key(&backing)
    }

    /// Some backing is owed `pixmap`'s buffer.
    fn is_owed_src(&self, pixmap: u32) -> bool {
        self.owed_srcs.contains_key(&pixmap)
    }

    /// The backings owed a frame of `pixmap`.
    pub(crate) fn owed_from(&self, pixmap: u32) -> Vec<DrawableId> {
        if !self.is_owed_src(pixmap) {
            return Vec::new();
        }
        self.owed
            .iter()
            .filter(|(_, owed)| owed.src == pixmap)
            .map(|(&backing, _)| backing)
            .collect()
    }

    fn remove_owed(&mut self, backing: DrawableId) -> Option<Owed> {
        let owed = self.owed.remove(&backing)?;
        if let Some(count) = self.owed_srcs.get_mut(&owed.src) {
            *count -= 1;
            if *count == 0 {
                let _ = self.owed_srcs.remove(&owed.src);
            }
        }
        Some(owed)
    }

    /// `backing` is owed `owed` now, a present whose idle is `event`'s; the
    /// frame it replaces, if any, is owed no longer.
    pub(crate) fn owe(&mut self, backing: DrawableId, owed: Owed, event: CompletedPresentEvent) {
        debug_assert!(
            self.buffers.contains_key(&owed.src),
            "owed a buffer that is not recorded"
        );
        debug_assert!(!event.emit_idle, "a direct present's idle is held here");
        let _ = self.uncompleted.insert(event.present_id);
        self.held.entry(owed.src).or_default().push(event);
        let replaced = self.remove_owed(backing);
        *self.owed_srcs.entry(owed.src).or_default() += 1;
        let _ = self.owed.insert(backing, owed);
        if let Some(replaced) = replaced {
            self.release_idles(replaced.src);
        }
    }

    /// The frame owed `backing` went to the host.
    pub(crate) fn handed(&mut self, backing: DrawableId) {
        let Some(owed) = self.owed.get_mut(&backing) else {
            debug_assert!(false, "handed a frame nothing is owed");
            return;
        };
        owed.handed = true;
        if let Some(buffer) = self.buffers.get_mut(&owed.src) {
            debug_assert!(
                !buffer.host_holds,
                "a buffer the host holds was handed again"
            );
            buffer.host_holds = true;
        }
    }

    /// The host gave the buffer of `generation` back.
    pub(crate) fn released(&mut self, generation: u64) {
        let Some(&pixmap) = self.pixmaps.get(&generation) else {
            // Its pixmap went first; its idles went with it.
            return;
        };
        if let Some(buffer) = self.buffers.get_mut(&pixmap) {
            buffer.host_holds = false;
        }
        self.release_idles(pixmap);
    }

    /// The frame owed `backing` was copied into it, by GPU work that
    /// finishes with `copy` (`None`: nothing was copied).
    pub(crate) fn settled(&mut self, backing: DrawableId, copy: Option<FenceTicket>) {
        let Some(owed) = self.remove_owed(backing) else {
            return;
        };
        if let (Some(buffer), Some(copy)) = (self.buffers.get_mut(&owed.src), copy) {
            buffer.read_by = Some(copy);
        }
        self.release_idles(owed.src);
    }

    /// The descriptor of `pixmap`'s buffer, given away once, for the host
    /// to make its object of.
    fn take_buffer(&mut self, pixmap: u32) -> Option<DirectBuffer> {
        let buffer = self.buffers.get_mut(&pixmap)?;
        let fd = buffer.fd.take()?;
        self.kept -= 1;
        Some(DirectBuffer {
            generation: buffer.generation,
            width: buffer.layout.width,
            height: buffer.layout.height,
            offset: buffer.layout.offset,
            stride: buffer.layout.stride,
            depth: buffer.layout.depth,
            fd,
        })
    }

    /// `pixmap`'s buffer went: the host may destroy its object of it, and
    /// the idles it held are due (after the GPU work that read it). The
    /// backings owed it must have been settled first.
    pub(crate) fn forget(&mut self, pixmap: u32) {
        debug_assert!(
            !self.is_owed_src(pixmap),
            "a buffer forgotten while a backing is owed it"
        );
        let after = self.buffers.remove(&pixmap).and_then(|buffer| {
            let _ = self.pixmaps.remove(&buffer.generation);
            if buffer.fd.is_some() {
                self.kept -= 1;
            } else {
                // Only a buffer the host was given has an object there.
                self.freed.push(buffer.generation);
            }
            buffer.read_by
        });
        let now = Instant::now();
        for event in self.held.remove(&pixmap).unwrap_or_default() {
            self.due.push(DueIdle {
                event,
                after: after.clone(),
                since: now,
            });
        }
    }

    /// The idles of `pixmap` are due when nothing can read its buffer: no
    /// backing is owed it and the host does not hold it.
    fn release_idles(&mut self, pixmap: u32) {
        if self.is_owed_src(pixmap) {
            return;
        }
        let after = match self.buffers.get(&pixmap) {
            Some(buffer) if buffer.host_holds => return,
            Some(buffer) => buffer.read_by.clone(),
            None => None,
        };
        let now = Instant::now();
        for event in self.held.remove(&pixmap).unwrap_or_default() {
            self.due.push(DueIdle {
                event,
                after: after.clone(),
                since: now,
            });
        }
    }

    /// The core drained the completion of present `present_id`.
    pub(crate) fn completed(&mut self, present_id: u64) {
        let _ = self.uncompleted.remove(&present_id);
    }

    /// The idles due at `now`: their completion drained, and the copy that
    /// read their buffer last `done`, or waited for [`READ_WAIT_MAX`].
    pub(crate) fn take_idles(
        &mut self,
        now: Instant,
        done: impl Fn(&FenceTicket) -> bool,
    ) -> Vec<CompletedPresentEvent> {
        if self.due.is_empty() {
            return Vec::new();
        }
        let (ready, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.due).into_iter().partition(|idle| {
                if self.uncompleted.contains(&idle.event.present_id) {
                    return false;
                }
                let Some(after) = &idle.after else {
                    return true;
                };
                if done(after) {
                    return true;
                }
                let late = now >= idle.since + READ_WAIT_MAX;
                if late {
                    log::warn!(
                        "rootless: the copy that read a direct frame's buffer has not finished \
                         in {READ_WAIT_MAX:?}; its idle goes anyway"
                    );
                }
                late
            });
        self.due = waiting;
        ready.into_iter().map(|idle| idle.event).collect()
    }

    /// When the loop must poll the GPU for an idle waiting on a copy.
    pub(crate) fn gpu_wait(&self) -> Option<Instant> {
        self.due
            .iter()
            .filter(|idle| idle.after.is_some())
            .map(|idle| idle.since + READ_WAIT_MAX)
            .min()
    }

    /// The generations whose host objects may go, taken.
    pub(crate) fn take_freed(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.freed)
    }

    fn host_holds(&self, pixmap: u32) -> bool {
        self.buffers
            .get(&pixmap)
            .is_some_and(|buffer| buffer.host_holds)
    }

    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self, by: Duration) {
        for owed in self.owed.values_mut() {
            owed.since -= by;
        }
    }
}

/// What the tick does with a frame owed a window's backing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Nothing now; the loop comes round by the deadline, if any.
    Keep(Option<Instant>),
    /// Hand it to the host for this window.
    Hand(DirectWindow),
    /// Copy it into the backing: the window is read back.
    Settle,
}

/// The one rule of the tick and of the loop's wakeup for a frame `owed`
/// the backing of `window` (`None`: the presenter shows no window with that
/// backing, unmapped or never mapped): they must agree, or the loop would
/// spin or a frame be stranded.
pub(crate) fn action(owed: &Owed, window: Option<&DirectWindow>, now: Instant) -> Action {
    let Some(window) = window else {
        return Action::Settle;
    };
    match window.readiness {
        // Shown nothing yet (a new window, or one made again): read back
        // whole, which the backing must hold the frame for.
        Readiness::ReadyFirst => Action::Settle,
        Readiness::Ready if owed.handed => Action::Keep(None),
        // A readback in flight finishes first; the next `Ready` takes it.
        Readiness::Ready if window.reading => Action::Keep(None),
        Readiness::Ready => Action::Hand(*window),
        Readiness::Setup | Readiness::Busy if owed.handed => Action::Keep(None),
        Readiness::Setup | Readiness::Busy => {
            let deadline = owed.since + OWED_FOR;
            if now >= deadline {
                Action::Settle
            } else {
                Action::Keep(Some(deadline))
            }
        }
    }
}

impl KmsBackend {
    fn presenter(&self) -> Option<&Presenter> {
        self.pstate.rootless.as_ref()?.presenter.as_ref()
    }

    /// The rootless host can show a client's buffer of `depth` as a
    /// window's.
    pub(in crate::kms::render::backend) fn host_takes_direct(&self, depth: u8) -> bool {
        self.presenter()
            .is_some_and(|presenter| presenter.host.takes_direct(depth))
    }

    /// A DRI3 buffer the client named LINEAR was imported as `pixmap`, `fd`
    /// a descriptor of it taken before the import: a whole-window present
    /// of it may go to the host as it is.
    pub(in crate::kms::render::backend) fn record_direct_buffer(
        &mut self,
        pixmap: u32,
        fd: std::io::Result<OwnedFd>,
        layout: Layout,
    ) {
        let fd = match fd {
            Ok(fd) => fd,
            Err(error) => {
                log::warn!(
                    "rootless: keeping DRI3 pixmap 0x{pixmap:x} for direct frames: {error}; \
                     its presents are copied"
                );
                return;
            }
        };
        if let Err(refused) = self.pstate.direct_frames.record(pixmap, fd, layout) {
            log::debug!(
                "rootless: DRI3 pixmap 0x{pixmap:x} takes no direct frames ({refused}); \
                 its presents are copied"
            );
        }
    }

    /// The top-level `paint_dst` covers alone, if it does and `backing` is
    /// that top-level's: `paint_dst` is the top-level (a child of the root),
    /// or the one viewable child at each level down from it, each at the
    /// origin with its parent's size and no border; and nothing viewable is
    /// inside it. A window with storage of its own below the top-level (one
    /// a client redirected) is no top-level. Whether the presenter shows the
    /// top-level is the caller's to check.
    ///
    /// The window tree is walked once for each window and backing while it
    /// does not change (`TrackedWindows::generation`).
    fn direct_top(&mut self, paint_dst: u32, backing: DrawableId) -> Option<u32> {
        let generation = self.windows.generation();
        let (at, cache) = &mut self.pstate.direct_frames.covers;
        if *at != generation {
            *at = generation;
            cache.clear();
        }
        if let Some(&top) = cache.get(&(paint_dst, backing)) {
            return top;
        }
        let top = self.walk_direct_top(paint_dst, backing);
        let _ = self
            .pstate
            .direct_frames
            .covers
            .1
            .insert((paint_dst, backing), top);
        top
    }

    fn walk_direct_top(&self, paint_dst: u32, backing: DrawableId) -> Option<u32> {
        let viewable_children = |of: u32| {
            self.windows
                .iter()
                .filter(move |(_, geom)| geom.parent == Some(of) && geom.viewable)
                .count()
        };
        if viewable_children(paint_dst) != 0 {
            return None;
        }
        let mut window = paint_dst;
        // Bounded by the tree's depth.
        for _ in 0..64 {
            let geom = self.windows.get(&window)?;
            if self
                .store
                .lookup(window)
                .and_then(|id| self.store.redirected_target(id))
                == Some(backing)
            {
                // The window whose storage this is: only a top-level's,
                // a child of the root.
                let root = self.core.window_id;
                return geom.parent.is_none_or(|p| p == root).then_some(window);
            }
            let parent = geom.parent?;
            let outer = self.windows.get(&parent)?;
            if (geom.x, geom.y, geom.width, geom.height, geom.border_width)
                != (0, 0, outer.width, outer.height, 0)
                || viewable_children(parent) != 1
            {
                return None;
            }
            window = parent;
        }
        None
    }

    /// A backing a client may read on its GPU, outside any request.
    fn backing_exported(&self, backing: DrawableId) -> bool {
        self.pstate.exported_dmabufs.contains_key(&backing)
            || self.store.is_exported(backing)
            || self
                .store
                .get(backing)
                .is_some_and(|drawable| drawable.storage.is_exportable())
    }

    /// A `PresentPixmap` of a recorded buffer that stands for the whole
    /// window of one of the presenter's top-levels: owed to the top-level's
    /// backing rather than copied, its completion queued as a copy's is,
    /// its idle held (the module doc). Whether it was taken; the core copies
    /// one that was not, which settles what the backing is owed first.
    pub(in crate::kms::render::backend) fn try_direct_frame(
        &mut self,
        candidate: &PresentScanoutCandidate,
        mut event: CompletedPresentEvent,
    ) -> bool {
        let Some(layout) = self.pstate.direct_frames.layout(candidate.src_host_xid) else {
            return false;
        };
        if candidate.explicit_sync
            || !candidate.update_is_full
            || candidate.valid_region_xid != 0
            || (candidate.x_off, candidate.y_off) != (0, 0)
            || candidate.options & PRESENT_OPTION_COPY != 0
            || (layout.width, layout.height) != (candidate.src_width, candidate.src_height)
        {
            return false;
        }
        let Some(target) = self.peek_paint_target(candidate.paint_dst_host_xid) else {
            return false;
        };
        let backing = target.backing_id();
        // The window's whole image, at the backing's origin: a buffer of
        // another size (a game's old one, after its window was resized) is
        // copied, so what the backing shows moves on with it.
        let fills = self.store.get(backing).is_some_and(|drawable| {
            drawable.depth == layout.depth
                && (
                    drawable.storage.extent.width,
                    drawable.storage.extent.height,
                ) == (u32::from(layout.width), u32::from(layout.height))
        });
        if target.offset() != (0, 0)
            || target.has_border_clip()
            || target.x11_depth() != layout.depth
            || !fills
            || self.backing_exported(backing)
            || self.store.lookup(candidate.src_host_xid).is_none()
        {
            return false;
        }
        let Some(top) = self.direct_top(candidate.paint_dst_host_xid, backing) else {
            return false;
        };
        if !self.presenter().is_some_and(|presenter| {
            presenter.shows(top) && presenter.host.takes_direct(layout.depth)
        }) {
            return false;
        }
        event.emit_idle = false;
        self.pstate.direct_frames.owe(
            backing,
            Owed {
                src: candidate.src_host_xid,
                dst: candidate.paint_dst_host_xid,
                top,
                width: layout.width,
                height: layout.height,
                since: Instant::now(),
                handed: false,
            },
            event.clone(),
        );
        self.backend_present_enqueue_present_completion(event, candidate.completion_dst_host_xid);
        true
    }

    /// Copy the frame owed `backing` into it, as the present's copy would
    /// have; nothing when nothing is owed.
    pub(in crate::kms::render::backend) fn settle_direct(&mut self, backing: DrawableId) {
        let Some(owed) = self.pstate.direct_frames.owed(backing) else {
            return;
        };
        let copied = match self.store.lookup(owed.src) {
            Some(src) if self.store.get(backing).is_some() => {
                let rect = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D::default(),
                    extent: ash::vk::Extent2D {
                        width: u32::from(owed.width),
                        height: u32::from(owed.height),
                    },
                };
                match self.engine.copy_area(
                    &mut self.store,
                    &mut self.platform.gpu,
                    Src::server_internal(src),
                    Dst::server_internal(backing),
                    rect,
                    ash::vk::Offset2D::default(),
                ) {
                    Ok(()) => self
                        .store
                        .get(backing)
                        .and_then(|drawable| drawable.last_render_ticket.clone()),
                    Err(error) => {
                        log::warn!(
                            "rootless: settling the direct frame of 0x{:x} into window 0x{:x}: \
                             {error:?}; the window keeps its earlier content",
                            owed.src,
                            owed.dst
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        self.pstate.direct_frames.settled(backing, copied);
    }

    /// `Platform::settle_for_access`: the backing `host_xid`'s pixels are
    /// in (by its XID when `backing` is `None`) settled, and for a write
    /// every backing owed `host_xid` itself (a presented buffer drawn
    /// into). Nothing owed is the common case: one check.
    pub(in crate::kms::render::backend) fn settle_direct_access(
        &mut self,
        host_xid: u32,
        backing: Option<DrawableId>,
        access: Access,
    ) {
        let direct = &self.pstate.direct_frames;
        if !direct.is_owed_anything() {
            return;
        }
        if let Some(backing) = backing.or_else(|| self.store.lookup(host_xid))
            && direct.is_owed(backing)
        {
            self.settle_direct(backing);
        }
        if access == Access::Write {
            for backing in self.pstate.direct_frames.owed_from(host_xid) {
                self.settle_direct(backing);
            }
        }
    }

    /// A pixmap goes: the backings owed its buffer are settled while it can
    /// still be read, and the host's objects of it go too.
    pub(in crate::kms::render::backend) fn forget_direct_pixmap(&mut self, host_xid: u32) {
        if self.pstate.direct_frames.layout(host_xid).is_none() {
            return;
        }
        for backing in self.pstate.direct_frames.owed_from(host_xid) {
            self.settle_direct(backing);
        }
        self.pstate.direct_frames.forget(host_xid);
    }

    /// What the tick does with the frame `owed` the backing `backing`: by
    /// the presenter's window for the top-level the frame was owed for,
    /// while that top-level still has this backing.
    fn direct_action(
        &self,
        presenter: &Presenter,
        backing: DrawableId,
        owed: &Owed,
        now: Instant,
    ) -> Action {
        let has_backing = self
            .core
            .host_window_to_backing
            .get(&owed.top)
            .and_then(|pixmap| self.store.lookup(pixmap.as_raw()))
            == Some(backing);
        let window = presenter.direct_window(owed.top).filter(|_| has_backing);
        action(owed, window.as_ref(), now)
    }

    /// The presenter's tick, before it reads windows back: each window ready
    /// for a frame that is owed one gets it as a direct frame, and one that
    /// does not take it is settled, to be read back.
    pub(in crate::kms::render::backend) fn hand_direct_frames(
        &mut self,
        presenter: &mut Presenter,
    ) {
        let freed = self.pstate.direct_frames.take_freed();
        if !freed.is_empty() {
            presenter.host.forget_direct(&freed);
        }
        if self.pstate.direct_frames.is_owed_anything() {
            let now = Instant::now();
            let owed: Vec<(DrawableId, Owed)> = self
                .pstate
                .direct_frames
                .owed
                .iter()
                .map(|(&backing, &owed)| (backing, owed))
                .collect();
            for (backing, owed) in owed {
                match self.direct_action(presenter, backing, &owed, now) {
                    Action::Keep(_) => {}
                    Action::Settle => self.settle_direct(backing),
                    Action::Hand(window) => {
                        if self.hand_direct_frame(presenter.host.as_mut(), backing, &owed, window) {
                            presenter.direct_submitted(&mut self.store, window.id);
                        } else {
                            self.settle_direct(backing);
                        }
                    }
                }
            }
        }
        // A settle's copy waits in the open frame for whatever submits it
        // next, and an idle waits for that copy: submit it now, so neither
        // depends on another window's paint.
        if self.pstate.direct_frames.gpu_wait().is_some() {
            self.flush_before_damage_exposure();
        }
    }

    /// Hand the frame `owed` the backing `backing` to `host` for `window`,
    /// the buffer itself first if the host has not had it; whether the
    /// host took the frame.
    fn hand_direct_frame(
        &mut self,
        host: &mut dyn RootlessHost,
        backing: DrawableId,
        owed: &Owed,
        window: DirectWindow,
    ) -> bool {
        // One release answers one hand-over: a buffer the host holds is
        // not handed again until it was given back.
        if self.pstate.direct_frames.host_holds(owed.src)
            || self.backing_exported(backing)
            || self.direct_top(owed.dst, backing) != Some(owed.top)
        {
            return false;
        }
        let Some(generation) = self
            .pstate
            .direct_frames
            .buffers
            .get(&owed.src)
            .map(|buffer| buffer.generation)
        else {
            return false;
        };
        if let Some(buffer) = self.pstate.direct_frames.take_buffer(owed.src) {
            host.import_direct(buffer);
        }
        let taken = host.submit_direct(DirectFrame {
            id: window.id,
            generation,
            width: owed.width,
            height: owed.height,
        });
        if taken {
            self.pstate.direct_frames.handed(backing);
        }
        taken
    }

    /// When the loop must come round for frames owed: now for one the tick
    /// would hand or settle, else the earliest deadline; and the GPU poll
    /// while an idle waits for a settle's copy.
    pub(in crate::kms::render::backend) fn direct_next_wakeup(
        &self,
        presenter: &Presenter,
        now: Instant,
    ) -> Option<Instant> {
        let direct = &self.pstate.direct_frames;
        let owed = direct
            .owed
            .iter()
            .filter_map(|(&backing, owed)| {
                match self.direct_action(presenter, backing, owed, now) {
                    Action::Keep(deadline) => deadline,
                    Action::Hand(_) | Action::Settle => Some(now),
                }
            })
            .min();
        let gpu = direct
            .gpu_wait()
            .map(|deadline| deadline.min(now + crate::rootless::READBACK_POLL));
        owed.into_iter().chain(gpu).min()
    }

    /// The idles of direct presents due now. A copy the renderer will never
    /// finish (it failed, or there is none) holds none back.
    pub(in crate::kms::render::backend) fn take_direct_idles(
        &mut self,
    ) -> Vec<CompletedPresentEvent> {
        let vk = self.platform.gpu.vk().cloned();
        let failed = self.platform.gpu.renderer_failed || vk.is_none();
        self.pstate
            .direct_frames
            .take_idles(Instant::now(), |ticket| {
                failed || vk.as_ref().is_some_and(|vk| ticket.poll_signaled(vk))
            })
    }
}

#[cfg(test)]
pub(in crate::kms::render::backend) mod tests {
    use yserver_core::backend::PresentWake;

    use super::*;

    fn event(present_id: u64) -> CompletedPresentEvent {
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: 1,
            host_xid: 0,
            dst_host_xid: 0,
            options: 0,
            present_id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: false,
        }
    }

    fn owed(src: u32) -> Owed {
        Owed {
            src,
            dst: 9,
            top: 9,
            width: 4,
            height: 4,
            since: Instant::now(),
            handed: false,
        }
    }

    /// Frames with buffers recorded without a descriptor check: these
    /// tests are of the bookkeeping, `check_buffer`'s are below.
    fn frames(pixmaps: &[u32]) -> DirectFrames {
        let mut direct = DirectFrames::default();
        for &pixmap in pixmaps {
            direct.next_generation += 1;
            let fd = OwnedFd::from(std::fs::File::open("/dev/null").expect("/dev/null"));
            let generation = direct.next_generation;
            let _ = direct.pixmaps.insert(generation, pixmap);
            direct.kept += 1;
            let _ = direct.buffers.insert(
                pixmap,
                Buffer {
                    fd: Some(fd),
                    layout: Layout {
                        offset: 0,
                        stride: 16,
                        width: 4,
                        height: 4,
                        depth: 24,
                    },
                    generation,
                    host_holds: false,
                    read_by: None,
                },
            );
        }
        direct
    }

    fn idled(direct: &mut DirectFrames) -> Vec<u64> {
        let mut ids: Vec<u64> = direct
            .take_idles(Instant::now(), |_| true)
            .into_iter()
            .map(|event| event.present_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn generation(direct: &DirectFrames, pixmap: u32) -> u64 {
        direct.buffers[&pixmap].generation
    }

    /// The host's release alone does not make a buffer idle while the
    /// backing is still owed it; settling it then does, and so does a
    /// newer present replacing it.
    #[test]
    fn a_buffer_the_host_gave_back_is_idle_only_once_no_backing_is_owed_it() {
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa, 0xb]);
        direct.owe(backing, owed(0xa), event(1));
        direct.completed(1);
        direct.handed(backing);
        direct.released(generation(&direct, 0xa));
        assert_eq!(idled(&mut direct), [] as [u64; 0], "still owed");
        direct.settled(backing, None);
        assert_eq!(idled(&mut direct), [1]);

        // Handed, then replaced by B before the host gave A back: A's idle
        // waits for the release, B's for its own.
        direct.owe(backing, owed(0xa), event(2));
        direct.completed(2);
        direct.handed(backing);
        direct.owe(backing, owed(0xb), event(3));
        direct.completed(3);
        assert_eq!(idled(&mut direct), [] as [u64; 0], "the host holds A");
        direct.released(generation(&direct, 0xa));
        assert_eq!(idled(&mut direct), [2]);
    }

    /// A present replaced before it was handed is idle at once; one of the
    /// same buffer as its successor waits for the successor's.
    #[test]
    fn a_frame_replaced_unshown_is_idle_unless_its_buffer_is_still_owed() {
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa, 0xb]);
        direct.owe(backing, owed(0xa), event(1));
        direct.owe(backing, owed(0xb), event(2));
        direct.completed(1);
        direct.completed(2);
        assert_eq!(idled(&mut direct), [1]);
        // B again: its first present waits, the buffer is still owed.
        direct.owe(backing, owed(0xb), event(3));
        direct.completed(3);
        assert_eq!(idled(&mut direct), [] as [u64; 0]);
        direct.settled(backing, None);
        assert_eq!(idled(&mut direct), [2, 3]);
    }

    /// No idle before its completion was drained: the completion holds the
    /// wake the idle releases.
    #[test]
    fn an_idle_waits_for_its_completion() {
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa, 0xb]);
        direct.owe(backing, owed(0xa), event(1));
        direct.owe(backing, owed(0xb), event(2));
        assert_eq!(idled(&mut direct), [] as [u64; 0]);
        direct.completed(1);
        assert_eq!(idled(&mut direct), [1]);
    }

    /// The buffer going makes its held idles due exactly once, and tells
    /// the host its object may go; a late release of it changes nothing.
    #[test]
    fn a_buffer_that_goes_idles_once_and_frees_the_hosts_object() {
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa]);
        direct.owe(backing, owed(0xa), event(1));
        direct.completed(1);
        direct.handed(backing);
        let gone = generation(&direct, 0xa);
        assert!(direct.take_buffer(0xa).is_some(), "the host is given it");
        assert!(direct.take_buffer(0xa).is_none(), "once");
        assert_eq!(direct.kept(), 0);
        direct.settled(backing, None);
        assert_eq!(idled(&mut direct), [] as [u64; 0], "the host holds it");
        direct.forget(0xa);
        assert_eq!(idled(&mut direct), [1]);
        assert_eq!(direct.take_freed(), [gone]);
        direct.released(gone);
        assert_eq!(idled(&mut direct), [] as [u64; 0]);
        assert!(direct.held.is_empty() && direct.due.is_empty());
    }

    /// An idle waiting on a copy the GPU never reports done goes after
    /// `READ_WAIT_MAX` all the same, and is not polled for after.
    #[test]
    fn an_idle_waits_for_its_copy_only_so_long() {
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa]);
        direct.owe(backing, owed(0xa), event(1));
        direct.completed(1);
        direct.settled(backing, Some(FenceTicket::for_tests_unsignaled()));
        let now = Instant::now();
        assert!(direct.take_idles(now, |_| false).is_empty());
        assert!(direct.gpu_wait().is_some());
        assert_eq!(direct.take_idles(now, |_| true).len(), 1, "the copy done");
        // A copy that never finishes.
        direct.owe(backing, owed(0xa), event(2));
        direct.completed(2);
        direct.settled(backing, Some(FenceTicket::for_tests_unsignaled()));
        assert!(direct.take_idles(now, |_| false).is_empty());
        let late = Instant::now() + READ_WAIT_MAX;
        assert_eq!(direct.take_idles(late, |_| false).len(), 1);
        assert_eq!(direct.gpu_wait(), None);
    }

    /// A copy the renderer will never finish (there is none here, as after
    /// a failure) holds no idle back.
    #[test]
    fn a_copy_the_renderer_will_never_finish_holds_no_idle_back() {
        let mut b = KmsBackend::for_tests();
        let backing = DrawableId::for_tests(1);
        let mut direct = frames(&[0xa]);
        direct.owe(backing, owed(0xa), event(1));
        direct.completed(1);
        direct.settled(backing, Some(FenceTicket::for_tests_unsignaled()));
        b.pstate.direct_frames = direct;
        let idled: Vec<u64> = b
            .take_direct_idles()
            .into_iter()
            .map(|event| event.present_id)
            .collect();
        assert_eq!(idled, [1]);
        b.set_renderer_failed_for_tests(true);
        assert!(b.pstate.direct_frames.gpu_wait().is_none());
    }

    /// The tick's rule: hand a frame to a window ready for one, settle one
    /// for a window that shows nothing yet or has no window, keep it for a
    /// busy window until the deadline, and never hand one twice.
    #[test]
    fn what_the_tick_does_with_an_owed_frame() {
        let now = Instant::now();
        let window = |readiness, reading| DirectWindow {
            id: yserver_core::rootless::ToplevelId::fake(0x40_0001),
            host_xid: 7,
            readiness,
            reading,
        };
        let fresh = Owed {
            since: now,
            ..owed(1)
        };
        let ready = window(Readiness::Ready, false);
        assert_eq!(action(&fresh, Some(&ready), now), Action::Hand(ready));
        assert_eq!(
            action(&fresh, Some(&window(Readiness::Ready, true)), now),
            Action::Keep(None)
        );
        assert_eq!(
            action(&fresh, Some(&window(Readiness::ReadyFirst, false)), now),
            Action::Settle
        );
        assert_eq!(action(&fresh, None, now), Action::Settle);
        let busy = window(Readiness::Busy, false);
        assert_eq!(
            action(&fresh, Some(&busy), now),
            Action::Keep(Some(now + OWED_FOR))
        );
        assert_eq!(action(&fresh, Some(&busy), now + OWED_FOR), Action::Settle);
        let shown = Owed {
            handed: true,
            ..fresh
        };
        assert_eq!(action(&shown, Some(&ready), now), Action::Keep(None));
        assert_eq!(
            action(&shown, Some(&busy), now + OWED_FOR),
            Action::Keep(None)
        );
        // Made again: shown nothing, so the backing must hold it.
        assert_eq!(
            action(&shown, Some(&window(Readiness::ReadyFirst, false)), now),
            Action::Settle
        );
    }

    /// What a client's descriptor must be: a dma-buf at least as large as
    /// its layout. A memfd, which the server could map and a client shrink,
    /// is not one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_buffer_that_is_not_a_large_enough_dma_buf_is_refused() {
        let layout = Layout {
            offset: 0,
            stride: 64,
            width: 16,
            height: 16,
            depth: 32,
        };
        let memfd =
            nix::sys::memfd::memfd_create(c"direct-test", nix::sys::memfd::MFdFlags::empty())
                .expect("memfd");
        nix::unistd::ftruncate(&memfd, 1 << 20).expect("ftruncate");
        assert_eq!(check_buffer(&memfd, layout), Err(Refused::NotDmaBuf));
        assert_eq!(
            check_buffer(
                &memfd,
                Layout {
                    depth: 16,
                    ..layout
                }
            ),
            Err(Refused::Depth(16))
        );
        assert_eq!(
            check_buffer(
                &memfd,
                Layout {
                    stride: 60,
                    ..layout
                }
            ),
            Err(Refused::Stride {
                stride: 60,
                width: 16
            })
        );
        assert_eq!(
            check_buffer(
                &memfd,
                Layout {
                    height: 0,
                    ..layout
                }
            ),
            Err(Refused::Empty)
        );
        // An extent past 32 bits, which a compositor may refuse fatally.
        assert_eq!(
            check_buffer(
                &memfd,
                Layout {
                    offset: u32::MAX - 100,
                    ..layout
                }
            ),
            Err(Refused::Range(u64::from(u32::MAX - 100) + 1024))
        );
        assert_eq!(
            check_buffer(
                &memfd,
                Layout {
                    stride: 1 << 28,
                    ..layout
                }
            ),
            Err(Refused::Range(1 << 32))
        );
    }

    /// A dma-buf at least as large as its layout is kept; a smaller one is
    /// not.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs /dev/udmabuf, as the live Vulkan ICD tests do"]
    fn a_dma_buf_is_kept_only_as_large_as_its_layout() {
        let layout = Layout {
            offset: 0,
            stride: 64,
            width: 16,
            height: 16,
            depth: 32,
        };
        let dmabuf = udmabuf(4096).expect("/dev/udmabuf");
        // 16 rows of 64 bytes: 1024 bytes, in a 4096-byte dma-buf.
        assert_eq!(check_buffer(&dmabuf, layout), Ok(()));
        assert_eq!(
            check_buffer(
                &dmabuf,
                Layout {
                    offset: 3200,
                    ..layout
                }
            ),
            Err(Refused::TooSmall {
                size: 4096,
                needs: 4224
            })
        );
        assert_eq!(
            check_buffer(
                &dmabuf,
                Layout {
                    stride: 1024,
                    width: 256,
                    ..layout
                }
            ),
            Err(Refused::TooSmall {
                size: 4096,
                needs: 16384
            })
        );
        // At most `KEPT_MAX` descriptors are kept.
        let mut direct = DirectFrames::default();
        for pixmap in 0..u32::try_from(KEPT_MAX).unwrap() {
            let fd = udmabuf(4096).expect("/dev/udmabuf");
            assert_eq!(direct.record(pixmap, fd, layout), Ok(()));
        }
        let fd = udmabuf(4096).expect("/dev/udmabuf");
        assert_eq!(direct.record(0xffff, fd, layout), Err(Refused::TooMany));
        // One handed over frees a place.
        assert!(direct.take_buffer(0).is_some());
        let fd = udmabuf(4096).expect("/dev/udmabuf");
        assert_eq!(direct.record(0xffff, fd, layout), Ok(()));
    }

    /// A dma-buf of `size` bytes from `/dev/udmabuf`, over a sealed memfd;
    /// `None` where the device is not there or not open to this user, and
    /// off Linux, which alone has it.
    pub(in crate::kms::render::backend) fn udmabuf(size: u64) -> Option<OwnedFd> {
        udmabuf_with_memfd(size).map(|(dmabuf, _)| dmabuf)
    }

    /// [`udmabuf`], and the memfd whose pages it is: what writes its
    /// pixels.
    #[cfg(not(target_os = "linux"))]
    pub(in crate::kms::render::backend) fn udmabuf_with_memfd(
        _size: u64,
    ) -> Option<(OwnedFd, OwnedFd)> {
        None
    }

    /// [`udmabuf`], and the memfd whose pages it is: what writes its
    /// pixels.
    #[cfg(target_os = "linux")]
    pub(in crate::kms::render::backend) fn udmabuf_with_memfd(
        size: u64,
    ) -> Option<(OwnedFd, OwnedFd)> {
        use nix::fcntl::{FcntlArg, SealFlag, fcntl};
        /// `struct udmabuf_create` (`include/uapi/linux/udmabuf.h`).
        #[repr(C)]
        struct Create {
            memfd: u32,
            flags: u32,
            offset: u64,
            size: u64,
        }
        nix::ioctl_write_ptr!(udmabuf_create, b'u', 0x42, Create);
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/udmabuf")
            .ok()?;
        let memfd = nix::sys::memfd::memfd_create(
            c"direct-udmabuf",
            nix::sys::memfd::MFdFlags::MFD_ALLOW_SEALING,
        )
        .ok()?;
        nix::unistd::ftruncate(&memfd, i64::try_from(size).ok()?).ok()?;
        fcntl(&memfd, FcntlArg::F_ADD_SEALS(SealFlag::F_SEAL_SHRINK)).ok()?;
        let create = Create {
            memfd: u32::try_from(std::os::fd::AsRawFd::as_raw_fd(&memfd)).ok()?,
            flags: 0,
            offset: 0,
            size,
        };
        // SAFETY: `create` is a valid `udmabuf_create` for the ioctl to
        // read; the device and the memfd are open.
        let fd =
            unsafe { udmabuf_create(std::os::fd::AsRawFd::as_raw_fd(&device), &raw const create) }
                .ok()?;
        // SAFETY: the ioctl returned a new descriptor this process owns.
        let dmabuf = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        Some((dmabuf, memfd))
    }
}
