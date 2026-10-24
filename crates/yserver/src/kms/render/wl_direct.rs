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
//! ([`WlDirect::queued`]) and shown first, so pacing drops none.
//! `YSERVER_PAIR_PACING=0` turns it off.

use std::collections::{HashMap, VecDeque};
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
    /// The second of a pair: not to be handed to the compositor before
    /// this `CLOCK_MONOTONIC` microsecond ([`Pacer`]).
    pub(crate) not_before: Option<u64>,
    /// The link handed it to the compositor
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

/// Times the second present of each pair of a window's ([`WlDirect`]'s
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
pub(crate) struct WlDirect {
    /// By pixmap host XID.
    pub(crate) layouts: HashMap<u32, Dri3Layout>,
    /// By the backing owed.
    pub(crate) owed: HashMap<DrawableId, Owed>,
    /// Frames a newer present replaced in [`WlDirect::owed`] before they
    /// were handed over, oldest first, when pacing is why they were not:
    /// a held second of a pair, or the first of a pair whose second came
    /// before the link synced. Shown in turn, then the newest.
    pub(crate) queued: HashMap<DrawableId, VecDeque<Owed>>,
    /// By the backing presented to.
    pacers: HashMap<DrawableId, Pacer>,
    pacing: Option<bool>,
    /// Generations of layouts whose pixmap went, for the link to destroy
    /// its buffers of.
    pub(crate) freed: Vec<u64>,
    next_generation: u64,
    next_serial: u64,
    enabled: Option<bool>,
    /// A present was owed since the link last synced: the loop comes round
    /// at once to hand it to the compositor (`KmsBackend::next_wakeup`).
    /// Executed by the core's due pass, which runs after this iteration's
    /// sync, it would otherwise wait for whatever woke the loop next -- the
    /// next MSC, often -- and be replaced by that MSC's present unshown.
    pub(crate) unsynced: bool,
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

    /// Whether the second of a pair is spaced from the first
    /// (`YSERVER_PAIR_PACING` is not `0`), read once.
    fn pacing(&mut self) -> bool {
        *self.pacing.get_or_insert_with(|| {
            let on = std::env::var("YSERVER_PAIR_PACING").map_or(true, |value| value != "0");
            log::info!(
                "wayland: pair pacing {}",
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
            yserver_core::core_loop::process_request::pace_instr_ms(),
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
        if let Some(old) = self.owed.insert(backing, owed)
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
                // the compositor takes it.
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
            }
        }
    }

    /// The frame owed `backing` the link is to show next: a queued one
    /// before the newest.
    pub(crate) fn next_owed(&self, backing: DrawableId) -> Option<Owed> {
        self.queued
            .get(&backing)
            .and_then(VecDeque::front)
            .or_else(|| self.owed.get(&backing))
            .copied()
    }

    /// The link handed the frame `serial` owed `backing` to the compositor.
    pub(crate) fn handed(&mut self, backing: DrawableId, serial: u64) {
        if let Some(queue) = self.queued.get_mut(&backing)
            && queue.front().is_some_and(|queued| queued.serial == serial)
        {
            let _ = queue.pop_front();
            if queue.is_empty() {
                let _ = self.queued.remove(&backing);
            }
        } else if let Some(owed) = self.owed.get_mut(&backing)
            && owed.serial == serial
        {
            owed.handed = true;
        }
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
        let _ = self.queued.remove(&backing);
    }

    /// `backing`'s window went: its pacing too.
    pub(crate) fn forget_backing(&mut self, backing: DrawableId) {
        let _ = self.queued.remove(&backing);
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

    #[test]
    fn a_held_frame_a_newer_present_replaces_is_queued_and_shown_first() {
        let mut direct = WlDirect::default();
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
        let mut direct = WlDirect::default();
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
        // Held, then two more 4 ms apart before the link took either.
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
        let mut direct = WlDirect::default();
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
}
