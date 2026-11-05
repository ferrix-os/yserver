//! Direct frames: a whole-window present of a client's LINEAR DRI3 buffer
//! handed to the rootless host as the client's buffer instead of copied.
//!
//! A game's `PresentPixmap` of a whole-window DRI3 buffer would be copied
//! three times on its way to the host's window system: the client's buffer
//! into its pixmap, the pixmap into the window's backing, and the backing
//! read back into a shared-memory buffer. Where the host takes dma-bufs
//! (`RootlessHost::takes_direct`), such a present copies nothing: the
//! backing is *owed* the pixmap ([`Owed`]), and the host is handed the
//! client's own buffer for the window (`RootlessHost::submit_direct`). The
//! backing is settled -- the copy made after all -- the moment anything
//! reads it or a present that cannot go direct writes it
//! ([`KmsBackend::settle_direct`]).
//!
//! What it does not catch: a core drawing request other than `CopyArea`
//! into a window whose backing is owed draws over the frame before the last
//! direct one. The host keeps showing the client's buffer until the next
//! present, which settles or replaces it, so that is one frame of a window
//! drawn both ways.
//!
//! `YSERVER_DIRECT_FRAMES=0` turns it off.
//!
//! ## When a buffer is the client's again
//!
//! A present the host is handed is a flip, not a copy: the window system
//! reads the client's buffer when it draws, after the present completed.
//! The present's `IdleNotify` (and its idle fence) therefore waits until
//! the host gives the buffer back (`FrameEvent::DirectReleased`), as
//! Xwayland's flips wait, and is held here ([`DirectFrames::hold_idle`])
//! until then. A present that is never handed over -- settled into the
//! backing, replaced before the host took it -- has been copied or dropped
//! and is idle at once. Sent at completion, it let a game draw its next
//! frame into a buffer the compositor was still reading: rows of black
//! dashes, every few frames.
//!
//! ## Pairs presented together
//!
//! A game with frame generation (FSR3) presents a generated frame and a
//! real one a millisecond or two apart, then nothing for the rest of the
//! real frame's time: at 34 frames a second shown, 17 ms and 42 ms apart in
//! turn. FSR3 spaces them itself only where the swapchain can say when a
//! frame is shown (`VK_EXT_present_timing`), which Mesa's X11 swapchain
//! cannot; neither Xwayland nor Mesa nor gamescope spaces them for it. So
//! the second of a pair is given a time not to be shown before
//! ([`Owed::not_before`]), the way FSR3's own pacer
//! (`FrameInterpolationSwapchainDX12.cpp`, `presenterThread`) times its
//! second present: half the moving average of the last ten real-frame
//! periods, less a tenth of their deviation and 0.1 ms, after the first
//! ([`Pacer`]). Its average is reset by a gap over 100 ms, as FSR3's is,
//! and by a frame that comes alone, so a pattern that breaks holds nothing
//! until ten pairs have come again. A frame that comes alone, the first of
//! a pair, and every frame of a game that does not present in pairs, is
//! never held. A held frame a newer present would replace is kept
//! ([`DirectFrames::queued`]) and shown first, so pacing drops none.
//! `YSERVER_PAIR_PACING=0` turns it off.

use std::{
    collections::{HashMap, VecDeque},
    os::fd::OwnedFd,
};

use yserver_core::backend::{CompletedPresentEvent, PresentScanoutCandidate};

use crate::{
    kms::render::{backend::KmsBackend, store::DrawableId},
    rootless::present_clock::monotonic_us,
};

/// A direct present's idle notification, held while the host may
/// still read its buffer.
#[derive(Debug, Clone)]
pub(crate) struct HeldIdle {
    /// The direct present's [`Owed::serial`].
    pub(crate) serial: u64,
    /// It was handed to the host, which now holds the buffer.
    pub(crate) handed: bool,
    /// Its completion, sent with `emit_idle` off; retired as an idle.
    pub(crate) event: CompletedPresentEvent,
}

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
    /// host's own buffer for it is kept by.
    pub(crate) generation: u64,
}

/// A window backing owed a present that was handed to the host
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
    /// The second of a pair: not to be handed to the host before
    /// this `CLOCK_MONOTONIC` microsecond ([`Pacer`]).
    pub(crate) not_before: Option<u64>,
    /// The host handed it to the host
    /// (`WindowImages::direct_handed`).
    pub(crate) handed: bool,
}

/// Presents to the second of a pair are this close to the first, in
/// microseconds: FSR3's two come 1 to 3 ms apart on the N150, a game
/// presenting each frame alone at 60 frames a second 16.7 ms apart.
const PAIR_GAP_US: u64 = 6_000;

/// Pairs this far apart reset the average, as FSR3's pacer does for a
/// frame slower than 10 a second.
const RESET_US: u64 = 100_000;

/// Pairs closer than this on average are not paced: the screen's refresh
/// spaces their frames as well as a hold could.
const MIN_PERIOD_US: u64 = 20_000;

/// The most frames queued behind a window's newest: a burst longer than
/// this loses its oldest, as one without pacing loses all but the newest.
const MAX_QUEUED: usize = 3;

/// The real-frame periods averaged, as FSR3's `SimpleMovingAverage<10>`.
const PERIODS: usize = 10;

/// Times the second present of each pair of a window's ([`DirectFrames`]'s
/// module doc).
#[derive(Debug, Default, Clone)]
pub(crate) struct Pacer {
    /// When the last present was owed.
    last: Option<u64>,
    /// When the first of the last pair was owed.
    first: Option<u64>,
    /// The last present was the second of a pair.
    last_second: bool,
    /// Pair-to-pair periods, microseconds, the newest at `next - 1`.
    periods: [u64; PERIODS],
    next: usize,
    count: usize,
}

impl Pacer {
    /// A present owed at `now`: when it is not to be shown before, if it is
    /// the second of a steady run of pairs.
    pub(crate) fn owe(&mut self, now: u64) -> Option<u64> {
        let gap = self.last.map(|last| now.saturating_sub(last));
        self.last = Some(now);
        if gap.is_some_and(|gap| gap < PAIR_GAP_US) && !self.last_second {
            self.last_second = true;
            let first = self.first?;
            let hold = self.hold()?;
            return Some(first + hold).filter(|&at| at > now);
        }
        // The first of a pair, or a frame alone.
        match self.first {
            Some(first) if self.last_second && now.saturating_sub(first) <= RESET_US => {
                self.periods[self.next] = now - first;
                self.next = (self.next + 1) % PERIODS;
                self.count = (self.count + 1).min(PERIODS);
            }
            _ => self.count = 0,
        }
        self.first = Some(now);
        self.last_second = false;
        None
    }

    /// How long after the first of a pair its second is shown: half the
    /// average period, less a tenth of the deviation and 0.1 ms
    /// (`conservativeAvg - qpcSafetyMargin` in FSR3's pacer). `None` until
    /// ten periods are known, or when they are too short to pace.
    fn hold(&self) -> Option<u64> {
        if self.count < PERIODS {
            return None;
        }
        let n = PERIODS as f64;
        let mean = self.periods.iter().sum::<u64>() as f64 / n;
        if mean < MIN_PERIOD_US as f64 {
            return None;
        }
        let deviation = (self
            .periods
            .iter()
            .map(|&p| (p as f64 - mean).powi(2))
            .sum::<f64>()
            / n)
            .sqrt();
        let hold = mean * 0.5 - deviation * 0.1 - 100.0;
        (hold > 0.0).then_some(hold as u64)
    }

    /// The average pair period in microseconds, for the log; 0 until known.
    pub(crate) fn period_us(&self) -> u64 {
        if self.count < PERIODS {
            return 0;
        }
        self.periods.iter().sum::<u64>() / PERIODS as u64
    }
}

/// The backend's side of zero-copy presents.
#[derive(Default)]
pub(crate) struct DirectFrames {
    /// By pixmap host XID.
    pub(crate) layouts: HashMap<u32, Dri3Layout>,
    /// By the backing owed.
    pub(crate) owed: HashMap<DrawableId, Owed>,
    /// Frames a newer present replaced in [`DirectFrames::owed`] before they
    /// were handed over, oldest first, when pacing is why they were not:
    /// a held second of a pair, or the first of a pair whose second came
    /// before the host synced. Shown in turn, then the newest.
    pub(crate) queued: HashMap<DrawableId, VecDeque<Owed>>,
    /// By the backing presented to.
    pacers: HashMap<DrawableId, Pacer>,
    pacing: Option<bool>,
    /// Generations of layouts whose pixmap went, for the host to destroy
    /// its buffers of.
    pub(crate) freed: Vec<u64>,
    next_generation: u64,
    next_serial: u64,
    enabled: Option<bool>,
    /// Idle notifications held for direct presents, by the presented
    /// pixmap's host XID (module doc, "When a buffer is the client's
    /// again").
    pub(crate) held_idle: HashMap<u32, Vec<HeldIdle>>,
    /// Idle notifications due now, for the core to retire
    /// (`Backend::drain_retired_present_idle_events`) once their
    /// completion has been drained.
    pub(crate) idle_now: Vec<CompletedPresentEvent>,
    /// Direct presents whose completion the core has not drained yet. An
    /// idle retired before it finds no wake to release: the idle fence
    /// never fires and the client waits on that buffer for ever (the N150,
    /// 2026-10-28: Endfield stopped after two minutes, a present replaced
    /// in the same pass it was owed).
    pub(crate) uncompleted: std::collections::HashSet<u64>,
    /// A present was owed since the host last synced: the loop comes round
    /// at once to hand it to the host (`KmsBackend::rootless_next_wakeup`).
    /// Executed by the core's due pass, which runs after this iteration's
    /// sync, it would otherwise wait for whatever woke the loop next -- the
    /// next MSC, often -- and be replaced by that MSC's present unshown.
    pub(crate) unsynced: bool,
}

impl DirectFrames {
    /// Whether direct presents are on (`YSERVER_WAYLAND_DIRECT` is not
    /// `0`), read once.
    pub(crate) fn enabled(&mut self) -> bool {
        *self.enabled.get_or_insert_with(|| {
            let on = std::env::var("YSERVER_DIRECT_FRAMES").map_or(true, |value| value != "0");
            log::info!(
                "rootless: zero-copy presents {}",
                if on {
                    "on"
                } else {
                    "off (YSERVER_DIRECT_FRAMES=0)"
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
    /// Whatever idles it still held are due: the pixmap is gone.
    pub(crate) fn forget(&mut self, pixmap: u32) {
        if let Some(old) = self.layouts.remove(&pixmap) {
            self.freed.push(old.generation);
        }
        if let Some(held) = self.held_idle.remove(&pixmap) {
            self.idle_now
                .extend(held.into_iter().map(|held| held.event));
        }
    }

    /// Whether the second of a pair is spaced from the first
    /// (`YSERVER_PAIR_PACING` is not `0`), read once.
    fn pacing(&mut self) -> bool {
        *self.pacing.get_or_insert_with(|| {
            let on = std::env::var("YSERVER_PAIR_PACING").map_or(true, |value| value != "0");
            log::info!(
                "rootless: pair pacing {}",
                if on {
                    "on"
                } else {
                    "off (YSERVER_PAIR_PACING=0)"
                }
            );
            on
        })
    }

    /// Owe `backing` a whole-window present of `src` to `dst`, at `now`
    /// (`CLOCK_MONOTONIC` microseconds).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn owe(
        &mut self,
        backing: DrawableId,
        src: u32,
        dst: u32,
        width: u16,
        height: u16,
        now: u64,
    ) {
        self.next_serial += 1;
        self.unsynced = true;
        let pacer = self.pacers.entry(backing).or_default();
        let not_before = pacer.owe(now);
        let period = pacer.period_us();
        let not_before = not_before.filter(|_| self.pacing());
        log::debug!(
            target: "present_pace",
            "WL-INSTR t={} stage=owe serial={} src=0x{src:x} hold_us={} period_us={period}",
            monotonic_us() / 1000,
            self.next_serial,
            not_before.map_or(0, |at| at - now),
        );
        let owed = Owed {
            src,
            dst,
            width,
            height,
            serial: self.next_serial,
            not_before,
            handed: false,
        };
        let replaced = self.owed.insert(backing, owed);
        if let Some(old) = replaced
            && !old.handed
            && old.not_before.is_none()
            && not_before.is_none()
        {
            // Replaced before the host took it, and not kept to show: the
            // compositor never gets that buffer, which is the client's
            // again.
            self.idle_unhanded(old.src, old.serial);
        }
        if let Some(old) = replaced
            && !old.handed
            && (old.not_before.is_some() || not_before.is_some())
        {
            // Held by the pacer and not shown yet, or the first of the pair
            // whose second is held, owed in the same iteration: it goes
            // first.
            let queue = self.queued.entry(backing).or_default();
            if !queue.is_empty() {
                // A third present before the pair was shown: the pattern
                // broke, and nothing waits any more -- each is shown as
                // the host takes it.
                for queued in queue.iter_mut() {
                    queued.not_before = None;
                }
                if let Some(newest) = self.owed.get_mut(&backing) {
                    newest.not_before = None;
                }
            }
            queue.push_back(old);
            if queue.len() > MAX_QUEUED
                && let Some(dropped) = queue.pop_front()
            {
                log::debug!(
                    target: "present_pace",
                    "WL-INSTR stage=replaced serial={}",
                    dropped.serial
                );
                self.idle_unhanded(dropped.src, dropped.serial);
            }
        }
    }

    /// The frame owed `backing` the host is to show next: a queued one
    /// before the newest.
    pub(crate) fn next_owed(&self, backing: DrawableId) -> Option<Owed> {
        self.queued
            .get(&backing)
            .and_then(VecDeque::front)
            .or_else(|| self.owed.get(&backing))
            .copied()
    }

    /// The host handed the frame `serial` owed `backing` to the host.
    pub(crate) fn handed(&mut self, backing: DrawableId, serial: u64) {
        let mut src = None;
        if let Some(queue) = self.queued.get_mut(&backing)
            && queue.front().is_some_and(|queued| queued.serial == serial)
        {
            src = queue.pop_front().map(|queued| queued.src);
            if queue.is_empty() {
                let _ = self.queued.remove(&backing);
            }
        } else if let Some(owed) = self.owed.get_mut(&backing)
            && owed.serial == serial
        {
            owed.handed = true;
            src = Some(owed.src);
        }
        if let Some(held) = src
            .and_then(|src| self.held_idle.get_mut(&src))
            .and_then(|held| held.iter_mut().find(|held| held.serial == serial))
        {
            held.handed = true;
        }
    }

    /// Hold the idle notification of the direct present `serial` of
    /// `pixmap` until the host is done with its buffer.
    pub(crate) fn hold_idle(&mut self, pixmap: u32, serial: u64, event: CompletedPresentEvent) {
        self.held_idle.entry(pixmap).or_default().push(HeldIdle {
            serial,
            handed: false,
            event,
        });
    }

    /// The direct present `serial` of `pixmap` will never be handed to the
    /// compositor: it was copied into its backing or dropped, and its
    /// pixmap is idle now.
    pub(crate) fn idle_unhanded(&mut self, pixmap: u32, serial: u64) {
        let Some(held) = self.held_idle.get_mut(&pixmap) else {
            return;
        };
        if let Some(at) = held
            .iter()
            .position(|held| held.serial == serial && !held.handed)
        {
            self.idle_now.push(held.remove(at).event);
        }
        if held.is_empty() {
            let _ = self.held_idle.remove(&pixmap);
        }
    }

    /// The host gave `pixmap`'s buffer back: every present of it
    /// that was handed over is idle.
    pub(crate) fn buffer_returned(&mut self, pixmap: u32) {
        let Some(held) = self.held_idle.get_mut(&pixmap) else {
            return;
        };
        let (returned, kept): (Vec<_>, Vec<_>) = held.drain(..).partition(|held| held.handed);
        self.idle_now
            .extend(returned.into_iter().map(|held| held.event));
        if kept.is_empty() {
            let _ = self.held_idle.remove(&pixmap);
        } else {
            *held = kept;
        }
    }

    /// The pixmap whose buffer is of `generation`, while it is kept.
    pub(crate) fn pixmap_of(&self, generation: u64) -> Option<u32> {
        self.layouts
            .iter()
            .find(|(_, layout)| layout.generation == generation)
            .map(|(&pixmap, _)| pixmap)
    }

    /// Take `backing`'s newest owed present out, idling it if it was never
    /// handed over: the caller copies it into the backing or drops it.
    pub(crate) fn take_owed(&mut self, backing: DrawableId) -> Option<Owed> {
        let owed = self.owed.remove(&backing)?;
        if !owed.handed {
            self.idle_unhanded(owed.src, owed.serial);
        }
        Some(owed)
    }

    /// When the loop must come round to hand over a held frame: the
    /// earliest time after `now` one is not to be shown before.
    pub(crate) fn pace_wake(&self, now: u64) -> Option<u64> {
        self.owed
            .values()
            .chain(self.queued.values().flatten())
            .filter(|owed| !owed.handed)
            .filter_map(|owed| owed.not_before)
            .filter(|&at| at > now)
            .min()
    }

    /// `backing` was settled: a queued frame is not shown, the newest is
    /// drawn into it.
    pub(crate) fn drop_backing(&mut self, backing: DrawableId) {
        for queued in self.queued.remove(&backing).unwrap_or_default() {
            self.idle_unhanded(queued.src, queued.serial);
        }
    }

    /// `backing`'s window went: its pacing too.
    pub(crate) fn forget_backing(&mut self, backing: DrawableId) {
        self.drop_backing(backing);
        let _ = self.pacers.remove(&backing);
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

/// A frame owed a top-level's backing, as the presenter hands it on.
pub(crate) struct OwedFrame {
    pub(crate) generation: u64,
    pub(crate) serial: u64,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) offset: u32,
    pub(crate) stride: u32,
    pub(crate) depth: u8,
    pub(crate) fd: OwnedFd,
}

impl KmsBackend {
    /// Whether presents may go to the host direct: a host that takes them,
    /// and not turned off.
    pub(in crate::kms::render::backend) fn direct_frames_on(&mut self) -> bool {
        let takes = self.pstate.rootless.as_ref().is_some_and(|r| {
            r.presenter
                .as_ref()
                .is_some_and(|presenter| presenter.host.takes_direct())
        });
        takes && self.pstate.direct_frames.enabled()
    }

    /// A LINEAR DRI3 buffer was imported as `pixmap`: a
    /// whole-window present of it may go to the host as it is.
    pub(in crate::kms::render::backend) fn record_direct_layout(
        &mut self,
        pixmap: u32,
        fd: &OwnedFd,
        (offset, stride): (u32, u32),
        (width, height): (u16, u16),
        depth: u8,
    ) {
        if !self.direct_frames_on() {
            return;
        }
        if let Ok(fd) = fd.try_clone() {
            self.pstate
                .direct_frames
                .record(pixmap, fd, offset, stride, width, height, depth);
        }
    }

    /// Make the copy a direct present to `backing` skipped: the presented
    /// pixmap into the window, as `execute_present_pixmap_copy` would have.
    /// Nothing when nothing is owed.
    pub(in crate::kms::render::backend) fn settle_direct(&mut self, backing: DrawableId) {
        // A held frame a newer one follows is not drawn into the backing:
        // the newer one is.
        self.pstate.direct_frames.drop_backing(backing);
        let Some(owed) = self.pstate.direct_frames.take_owed(backing) else {
            return;
        };
        let (Some(src), Some(dst)) = (
            self.resolve_paint_target(owed.src),
            self.resolve_paint_target(owed.dst),
        ) else {
            return;
        };
        let (sx, sy) = src.offset();
        let (dx, dy) = dst.offset();
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: sx, y: sy },
            extent: ash::vk::Extent2D {
                width: u32::from(owed.width),
                height: u32::from(owed.height),
            },
        };
        if let Err(error) = self.engine.copy_area(
            &mut self.store,
            &mut self.platform.gpu,
            src.src(),
            dst.dst(),
            rect,
            ash::vk::Offset2D { x: dx, y: dy },
        ) {
            log::warn!(
                "rootless: settling the direct present of 0x{:x} into 0x{:x}: {error:?}",
                owed.src,
                owed.dst
            );
        }
    }

    /// [`Self::settle_direct`] for the backing window `host_xid` draws into,
    /// before it is read or drawn: a copy from or into it, a `GetImage`.
    pub(in crate::kms::render::backend) fn settle_direct_window(&mut self, host_xid: u32) {
        if self.pstate.direct_frames.owed.is_empty() {
            return;
        }
        if let Some(target) = self.resolve_paint_target(host_xid) {
            self.settle_direct(target.backing_id());
        }
    }

    /// Whether a present of `width` by `height` fills `backing`, whose
    /// storage is its window's size exactly: only such a present can stand
    /// for the window's whole image.
    fn direct_fills(&self, backing: DrawableId, width: u16, height: u16) -> bool {
        self.store.get(backing).is_some_and(|drawable| {
            drawable.storage.extent.width == u32::from(width)
                && drawable.storage.extent.height == u32::from(height)
        })
    }

    /// A pixmap goes: the backings owed it are settled while it can still
    /// be read, and the host's buffers of it go too.
    pub(in crate::kms::render::backend) fn forget_direct_pixmap(&mut self, host_xid: u32) {
        for backing in self.pstate.direct_frames.owed_from(host_xid) {
            self.settle_direct(backing);
        }
        self.pstate.direct_frames.forget(host_xid);
    }

    /// A `PresentPixmap` of a whole window from a LINEAR DRI3 buffer, to a
    /// host that takes direct frames: owed to the window's backing rather
    /// than copied, the completion queued as the copy's would be. Any other
    /// present settles what the backing is owed first, so it draws over the
    /// frame it follows. Whether the present was taken.
    pub(in crate::kms::render::backend) fn try_direct_frame(
        &mut self,
        candidate: &PresentScanoutCandidate,
        event: CompletedPresentEvent,
    ) -> bool {
        let Some(target) = self.resolve_paint_target(candidate.paint_dst_host_xid) else {
            return false;
        };
        let backing = target.backing_id();
        let eligible = self.direct_frames_on()
            && !candidate.explicit_sync
            && candidate.update_is_full
            && candidate.valid_region_xid == 0
            && (candidate.x_off, candidate.y_off) == (0, 0)
            && target.offset() == (0, 0)
            && !target.has_border_clip()
            // The whole window: a buffer smaller than its backing -- a
            // game's old size, after the host maximized its window and
            // before the game made buffers of the new one -- is copied into
            // it, as any other present, so that what the backing shows moves
            // on with it.
            && self.direct_fills(backing, candidate.src_width, candidate.src_height)
            && self
                .pstate
                .direct_frames
                .layouts
                .get(&candidate.src_host_xid)
                .is_some_and(|layout| {
                    (layout.width, layout.height) == (candidate.src_width, candidate.src_height)
                });
        if !eligible {
            self.settle_direct(backing);
            return false;
        }
        self.pstate.direct_frames.owe(
            backing,
            candidate.src_host_xid,
            candidate.paint_dst_host_xid,
            candidate.src_width,
            candidate.src_height,
            monotonic_us(),
        );
        // A flip: complete now, idle once the host is done with the buffer
        // (the module's "When a buffer is the client's again").
        let mut event = event;
        event.emit_idle = false;
        if let Some(serial) = self
            .pstate
            .direct_frames
            .owed
            .get(&backing)
            .map(|owed| owed.serial)
        {
            let _ = self
                .pstate
                .direct_frames
                .uncompleted
                .insert(event.present_id);
            self.pstate
                .direct_frames
                .hold_idle(candidate.src_host_xid, serial, event.clone());
        }
        self.backend_present_enqueue_present_completion(event, candidate.completion_dst_host_xid);
        true
    }

    /// Whether window `dst` covers the top-level `top` alone: it is `top`,
    /// or the one viewable child at each level down to it, each at the
    /// origin with its parent's size and no border, and nothing viewable
    /// inside it.
    fn direct_covers(&self, top: u32, dst: u32) -> bool {
        let viewable_children = |of: u32| {
            self.windows
                .iter()
                .filter(move |(_, geom)| geom.parent == Some(of) && geom.viewable)
        };
        let mut window = top;
        for _ in 0..64 {
            let mut children = viewable_children(window);
            let first = children.next();
            if window == dst {
                return first.is_none();
            }
            let (Some((&child, geom)), None) = (first, children.next()) else {
                return false;
            };
            let Some(parent) = self.windows.get(&window) else {
                return false;
            };
            if (geom.x, geom.y, geom.width, geom.height, geom.border_width)
                != (0, 0, parent.width, parent.height, 0)
            {
                return false;
            }
            window = child;
        }
        false
    }

    /// The frame owed the top-level `top`'s backing to hand the host now,
    /// if one is and it covers the window: not a second of a pair before its
    /// time, nor the one the host shows already (`shown`).
    pub(in crate::kms::render::backend) fn owed_direct_frame(
        &mut self,
        top: u32,
        shown: Option<u64>,
    ) -> Option<OwedFrame> {
        let backing = self.resolve_paint_target(top)?.backing_id();
        let owed = self.pstate.direct_frames.next_owed(backing)?;
        // The window presented to may have gone, and its backing's id been
        // reused: then the entry is stale.
        if self
            .resolve_paint_target(owed.dst)
            .map(crate::kms::render::target::PaintTarget::backing_id)
            != Some(backing)
        {
            let _ = self.pstate.direct_frames.take_owed(backing);
            self.pstate.direct_frames.forget_backing(backing);
            return None;
        }
        if shown == Some(owed.serial) {
            return None;
        }
        if owed.not_before.is_some_and(|at| monotonic_us() < at) {
            // The second of a pair, until its time: the first stays shown.
            return None;
        }
        let frame = self
            .pstate
            .direct_frames
            .layouts
            .get(&owed.src)
            .filter(|_| self.direct_covers(top, owed.dst))
            .and_then(|layout| {
                Some(OwedFrame {
                    generation: layout.generation,
                    serial: owed.serial,
                    width: owed.width,
                    height: owed.height,
                    offset: layout.offset,
                    stride: layout.stride,
                    depth: layout.depth,
                    fd: layout.fd.try_clone().ok()?,
                })
            });
        if frame.is_none() {
            // Other windows drawn inside it, or its buffer gone: the frame
            // reaches the host by the backing, read back.
            self.settle_direct(backing);
        }
        frame
    }

    /// The frame `serial` owed `top`'s backing went to the host.
    pub(in crate::kms::render::backend) fn direct_handed(&mut self, top: u32, serial: u64) {
        if let Some(target) = self.resolve_paint_target(top) {
            self.pstate
                .direct_frames
                .handed(target.backing_id(), serial);
        }
    }

    /// The host gave back the client buffer of `generation`.
    pub(in crate::kms::render::backend) fn direct_released(&mut self, generation: u64) {
        if let Some(pixmap) = self.pstate.direct_frames.pixmap_of(generation) {
            self.pstate.direct_frames.buffer_returned(pixmap);
        }
    }

    /// A direct present's idle only after its completion was drained: the
    /// completion retains the idle fence's wake that the idle releases, and
    /// CompleteNotify goes before IdleNotify.
    pub(in crate::kms::render::backend) fn take_direct_idles(
        &mut self,
    ) -> Vec<CompletedPresentEvent> {
        let direct = &mut self.pstate.direct_frames;
        let (early, due): (Vec<_>, Vec<_>) = std::mem::take(&mut direct.idle_now)
            .into_iter()
            .partition(|event| direct.uncompleted.contains(&event.present_id));
        direct.idle_now = early;
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FSR3 as the N150 shows it: a pair 2 ms apart every `period` µs.
    fn pairs(pacer: &mut Pacer, start: u64, period: u64, n: u64) -> Vec<Option<u64>> {
        let mut holds = Vec::new();
        for k in 0..n {
            let first = start + k * period;
            assert_eq!(pacer.owe(first), None, "pair {k}: a first is never held");
            holds.push(pacer.owe(first + 2_000).map(|at| at - first));
        }
        holds
    }

    #[test]
    fn the_second_of_a_steady_pair_is_shown_half_a_period_after_the_first() {
        let mut pacer = Pacer::default();
        let holds = pairs(&mut pacer, 1_000_000, 58_800, 20);
        // Ten periods are known after the eleventh pair's first.
        assert!(holds[..10].iter().all(Option::is_none), "{holds:?}");
        for hold in &holds[10..] {
            // 29.4 ms less 0.1 ms; the deviation is 0.
            assert_eq!(*hold, Some(29_300));
        }
        assert_eq!(pacer.period_us(), 58_800);
    }

    #[test]
    fn frames_that_come_alone_are_never_held() {
        let mut pacer = Pacer::default();
        // 30 and 60 frames a second, and an uneven game.
        let mut now = 0;
        for k in 0..300_u64 {
            now += [33_333, 16_667, 21_000 + (k * 7_919) % 9_000][(k / 100) as usize];
            assert_eq!(pacer.owe(now), None, "frame {k}");
        }
    }

    #[test]
    fn a_broken_pattern_holds_nothing_until_ten_pairs_came_again() {
        let mut pacer = Pacer::default();
        let _ = pairs(&mut pacer, 0, 58_000, 12);
        // A frame alone (frame generation off for a moment).
        assert_eq!(pacer.owe(12 * 58_000), None);
        let holds = pairs(&mut pacer, 12 * 58_000 + 30_000, 58_000, 12);
        assert!(holds[..10].iter().all(Option::is_none), "{holds:?}");
        assert!(holds[10].is_some());
        // A stall over 100 ms resets it too.
        let start = 12 * 58_000 + 30_000 + 12 * 58_000 + 150_000;
        let holds = pairs(&mut pacer, start, 58_000, 11);
        assert!(holds[..10].iter().all(Option::is_none), "{holds:?}");
    }

    #[test]
    fn a_second_already_late_is_not_held_and_short_periods_are_not_paced() {
        let mut pacer = Pacer::default();
        let _ = pairs(&mut pacer, 0, 40_000, 12);
        // The second comes 5 ms after the first, before its time: held to
        // 19.9 ms. A pair whose time is past is not held.
        let first = 12 * 40_000;
        assert_eq!(pacer.owe(first), None);
        assert_eq!(pacer.owe(first + 5_000), Some(first + 19_900));
        let mut fast = Pacer::default();
        // 150 pairs a second: the refresh spaces them.
        assert!(pairs(&mut fast, 0, 6_600, 30).iter().all(Option::is_none));
    }

    /// A completion of present `id`, its idle held back.
    fn completion(id: u64) -> CompletedPresentEvent {
        CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(1),
            serial: 1,
            host_xid: 0,
            dst_host_xid: 0,
            options: 0,
            present_id: id,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: false,
        }
    }

    /// Owe `backing` a present of `pixmap` with its idle held, as
    /// `KmsBackend::try_direct_frame` does; its serial.
    fn owe_held(direct: &mut DirectFrames, backing: DrawableId, pixmap: u32, now: u64) -> u64 {
        direct.owe(backing, pixmap, 9, 8, 8, now);
        let serial = direct.owed[&backing].serial;
        direct.hold_idle(pixmap, serial, completion(serial));
        serial
    }

    fn idled(direct: &mut DirectFrames) -> Vec<u64> {
        std::mem::take(&mut direct.idle_now)
            .into_iter()
            .map(|event| event.present_id)
            .collect()
    }

    /// 2026-10-28, Endfield on the N150: a zero-copy present was idle the
    /// moment it completed, so the game drew its next frame into a buffer
    /// hyprix was still reading -- black dashes in the loading screen, the
    /// HUD missing from a frame now and then. A handed buffer is idle only
    /// when the host gives it back; one never handed is idle at once.
    #[test]
    fn a_handed_buffer_is_idle_only_once_the_compositor_gives_it_back() {
        let mut direct = DirectFrames::default();
        let backing = DrawableId::for_tests(1);
        let a = owe_held(&mut direct, backing, 0xa, 0);
        direct.handed(backing, a);
        assert!(idled(&mut direct).is_empty(), "handed: the host reads it");
        // The next frame, of another buffer, is handed too; A is still
        // the host's until it says otherwise.
        let b = owe_held(&mut direct, backing, 0xb, 16_000);
        direct.handed(backing, b);
        assert!(idled(&mut direct).is_empty());
        direct.buffer_returned(0xa);
        assert_eq!(idled(&mut direct), [a]);
        // A present replaced before the host took it never reaches the
        // compositor: idle at once.
        let c = owe_held(&mut direct, backing, 0xc, 32_000);
        let _ = owe_held(&mut direct, backing, 0xa, 48_000);
        assert_eq!(idled(&mut direct), [c]);
        // Settled into the backing (copied), never handed: idle at once.
        let owed = direct.take_owed(backing).expect("owed");
        assert_eq!(idled(&mut direct), [owed.serial]);
        // B, still held, goes with its pixmap.
        direct.forget(0xb);
        assert_eq!(idled(&mut direct), [b]);
        assert!(direct.held_idle.is_empty());
    }

    #[test]
    fn a_held_frame_a_newer_present_replaces_is_queued_and_shown_first() {
        let mut direct = DirectFrames::default();
        let backing = DrawableId::for_tests(1);
        let mut now = 0;
        for _ in 0..11 {
            direct.owe(backing, 1, 9, 8, 8, now);
            let first = direct.next_owed(backing).expect("owed");
            direct.handed(backing, first.serial);
            direct.owe(backing, 2, 9, 8, 8, now + 2_000);
            let second = direct.next_owed(backing).expect("owed");
            direct.handed(backing, second.serial);
            now += 50_000;
        }
        direct.owe(backing, 1, 9, 8, 8, now);
        let first = direct.next_owed(backing).expect("owed");
        direct.handed(backing, first.serial);
        direct.owe(backing, 2, 9, 8, 8, now + 2_000);
        let held = direct.next_owed(backing).expect("owed");
        assert!(held.not_before.is_some());
        assert_eq!(direct.pace_wake(now + 2_000), held.not_before);
        // The next pair's first comes before the held one was shown.
        direct.owe(backing, 3, 9, 8, 8, now + 10_000);
        assert_eq!(
            direct.next_owed(backing).map(|o| o.serial),
            Some(held.serial)
        );
        direct.handed(backing, held.serial);
        let newest = direct.next_owed(backing).expect("owed");
        assert_eq!(newest.src, 3);
        assert!(direct.queued.is_empty());
    }

    #[test]
    fn a_burst_of_three_releases_the_hold_and_shows_each_in_turn() {
        let mut direct = DirectFrames::default();
        let backing = DrawableId::for_tests(1);
        let mut now = 0;
        for _ in 0..11 {
            direct.owe(backing, 1, 9, 8, 8, now);
            direct.handed(backing, direct.next_owed(backing).expect("owed").serial);
            direct.owe(backing, 2, 9, 8, 8, now + 2_000);
            direct.handed(backing, direct.next_owed(backing).expect("owed").serial);
            now += 50_000;
        }
        direct.owe(backing, 1, 9, 8, 8, now);
        direct.handed(backing, direct.next_owed(backing).expect("owed").serial);
        // Held, then two more 4 ms apart before the host took either.
        direct.owe(backing, 2, 9, 8, 8, now + 2_000);
        assert!(
            direct
                .next_owed(backing)
                .expect("owed")
                .not_before
                .is_some()
        );
        direct.owe(backing, 3, 9, 8, 8, now + 6_000);
        direct.owe(backing, 4, 9, 8, 8, now + 10_000);
        let mut shown = Vec::new();
        while let Some(owed) = direct.next_owed(backing) {
            if owed.handed {
                break;
            }
            assert_eq!(owed.not_before, None, "src {}: still held", owed.src);
            shown.push(owed.src);
            direct.handed(backing, owed.serial);
        }
        assert_eq!(shown, [2, 3, 4]);
        assert_eq!(direct.pace_wake(now), None);
    }

    #[test]
    fn a_pair_owed_before_the_link_synced_shows_its_first_then_holds_its_second() {
        let mut direct = DirectFrames::default();
        let backing = DrawableId::for_tests(1);
        let mut now = 0;
        for _ in 0..11 {
            direct.owe(backing, 1, 9, 8, 8, now);
            direct.handed(backing, direct.next_owed(backing).expect("owed").serial);
            direct.owe(backing, 2, 9, 8, 8, now + 2_000);
            direct.handed(backing, direct.next_owed(backing).expect("owed").serial);
            now += 50_000;
        }
        // Both fences signalled together: owed in one iteration.
        direct.owe(backing, 1, 9, 8, 8, now);
        direct.owe(backing, 2, 9, 8, 8, now + 100);
        let first = direct.next_owed(backing).expect("owed");
        assert_eq!((first.src, first.not_before), (1, None));
        direct.handed(backing, first.serial);
        let second = direct.next_owed(backing).expect("owed");
        assert_eq!(second.src, 2);
        assert!(second.not_before.is_some());
    }

    /// A direct present's idle waits for its completion to be drained: the
    /// completion retains the idle fence's wake that the idle releases.
    #[test]
    fn a_direct_presents_idle_waits_for_its_completion() {
        use yserver_core::backend::Backend as _;
        let mut b = KmsBackend::for_tests();
        let _ = b.pstate.direct_frames.uncompleted.insert(77);
        b.pstate.direct_frames.idle_now.push(completion(77));
        assert!(
            b.drain_retired_present_idle_events().is_empty(),
            "an idle before its completion releases no wake",
        );
        let _ = b.pstate.direct_frames.uncompleted.remove(&77);
        let idled = b.drain_retired_present_idle_events();
        assert_eq!(idled.iter().map(|e| e.present_id).collect::<Vec<_>>(), [77]);
    }
}
