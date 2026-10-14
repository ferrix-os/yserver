//! The Present clock of the rootless Wayland backend: the `(msc, ust)` the
//! root's one RandR output, `WAYLAND-1`, counts its frames by.
//!
//! A server that owns a DRM card counts the kernel's vblanks; a client of a
//! Wayland compositor has none to count. Xwayland counts the compositor's
//! frame callbacks instead, and keeps a timer at the output's refresh for
//! the time nothing is drawn (`xwl_present_timer_callback`), as Xorg's
//! `present_fake` does for a screen without a CRTC. This clock does both:
//!
//! * between frames it runs on its own, one MSC each refresh period of the
//!   compositor's screen (`1e6 / refresh` µs), so a `PresentNotifyMSC` for
//!   a later MSC always comes due, whether anything is drawn or not;
//! * a frame callback is the compositor's refresh, seen: the clock takes
//!   its time as the time of the MSC it falls in, or of the next one when it
//!   comes in the later half of a period, which keeps the count in step with
//!   the compositor's refresh rather than drifting from it.
//!
//! The UST is `CLOCK_MONOTONIC` in microseconds, as the kernel's vblank
//! timestamps are. Neither number ever goes back: a frame only moves the
//! clock's phase forward, and the MSC it is in stays the one it was.
//!
//! The clock is read on demand ([`FrameClock::sample`]), so nothing needs to
//! run for it to advance; what needs a timer is a waiter. While Present work
//! is parked on this output the core loop asks the backend to arm a wake
//! (`Backend::arm_idle_vblanks` and its siblings), which here is
//! [`FrameClock::arm`]: the loop then comes round at the next MSC, the core
//! fires what came due, and arms again for whatever is still parked.

/// The refresh a compositor that gave none is taken to have, in millihertz.
const DEFAULT_REFRESH_MHZ: u64 = 60_000;

/// One output's Present clock. Times are `CLOCK_MONOTONIC` microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameClock {
    /// The length of an MSC, in microseconds; never zero.
    period: u64,
    /// An MSC, and the time it began; the clock counts on from there.
    base_msc: u64,
    base_ust: u64,
    /// When the core loop must next come round for parked Present work:
    /// the time of an MSC still to come. `None` while nothing waits.
    wake: Option<u64>,
}

impl FrameClock {
    /// A clock at MSC 1 from `now`, at `refresh_mhz` millihertz (0 for the
    /// default 60 Hz). It starts at 1 rather than 0 because the core takes
    /// an MSC of 0 to mean an output with no clock.
    pub(crate) fn new(now: u64, refresh_mhz: u64) -> Self {
        Self {
            period: period_of(refresh_mhz),
            base_msc: 1,
            base_ust: now,
            wake: None,
        }
    }

    /// The length of an MSC, in microseconds.
    pub(crate) fn period(&self) -> u64 {
        self.period
    }

    /// The MSC `now` falls in, and the time it began.
    pub(crate) fn sample(&self, now: u64) -> (u64, u64) {
        let elapsed = now.saturating_sub(self.base_ust) / self.period;
        (
            self.base_msc.wrapping_add(elapsed),
            self.base_ust + elapsed * self.period,
        )
    }

    /// The compositor showed a frame at `now` (a frame callback came). In
    /// the first half of a period it is the MSC the clock is in, whose time
    /// becomes `now`; in the later half it is the next MSC, which begins
    /// now. Either way the clock counts on from `now`.
    pub(crate) fn frame(&mut self, now: u64) {
        let (msc, ust) = self.sample(now);
        if now < self.base_ust {
            return;
        }
        let into = now - ust;
        self.base_msc = if into * 2 >= self.period {
            msc.wrapping_add(1)
        } else {
            msc
        };
        self.base_ust = now;
        // A wake armed for the MSC that began early here would come a
        // little after it; one at the new phase is what the waiter wants.
        if self.wake.is_some() {
            self.wake = Some(self.next_tick(now));
        }
    }

    /// The compositor's screen refreshes at `refresh_mhz` millihertz from
    /// `now` on (0 for the default). The MSC and its time so far stay.
    pub(crate) fn set_refresh(&mut self, now: u64, refresh_mhz: u64) {
        let period = period_of(refresh_mhz);
        if period == self.period {
            return;
        }
        let (msc, ust) = self.sample(now);
        self.base_msc = msc;
        self.base_ust = ust;
        self.period = period;
    }

    /// Something waits on a later MSC: come round when the next one
    /// begins. Whether a wake was newly armed (`false` when one is armed
    /// already, so the core loop may call this every iteration).
    pub(crate) fn arm(&mut self, now: u64) -> bool {
        if self.wake.is_some_and(|wake| wake > now) {
            return false;
        }
        self.wake = Some(self.next_tick(now));
        true
    }

    /// When the core loop must come round, if anything waits.
    pub(crate) fn wake(&self) -> Option<u64> {
        self.wake
    }

    /// The loop came round at `now`: a wake that is due is spent, and the
    /// core arms again if anything still waits.
    pub(crate) fn expire(&mut self, now: u64) {
        if self.wake.is_some_and(|wake| wake <= now) {
            self.wake = None;
        }
    }

    /// The time the MSC after the one `now` falls in begins.
    fn next_tick(&self, now: u64) -> u64 {
        let (_, ust) = self.sample(now);
        ust.max(self.base_ust) + self.period
    }
}

/// An MSC's length in microseconds at `refresh_mhz` millihertz.
fn period_of(refresh_mhz: u64) -> u64 {
    let refresh_mhz = if refresh_mhz == 0 {
        DEFAULT_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    // 1e6 µs per second, 1e3 mHz per Hz.
    (1_000_000_000 / refresh_mhz).max(1)
}

/// `CLOCK_MONOTONIC` now, in microseconds: the clock the kernel stamps its
/// vblanks with, and so the one Present's UST is in.
pub(crate) fn monotonic_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable `timespec`, and CLOCK_MONOTONIC is
    // always there on Linux.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    u64::try_from(now.tv_sec).unwrap_or(0) * 1_000_000
        + u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ60: u64 = 60_000;

    #[test]
    fn the_period_is_the_refresh_in_microseconds() {
        assert_eq!(FrameClock::new(0, HZ60).period(), 16_666);
        assert_eq!(FrameClock::new(0, 59_951).period(), 16_680);
        assert_eq!(FrameClock::new(0, 144_000).period(), 6_944);
        assert_eq!(FrameClock::new(0, 0).period(), 16_666, "no rate is 60 Hz");
    }

    #[test]
    fn the_clock_counts_one_msc_a_period_without_frames() {
        let clock = FrameClock::new(1_000_000, HZ60);
        assert_eq!(clock.sample(1_000_000), (1, 1_000_000));
        assert_eq!(clock.sample(1_016_665), (1, 1_000_000));
        assert_eq!(clock.sample(1_016_666), (2, 1_016_666));
        // A second later: sixty more.
        assert_eq!(clock.sample(2_000_000), (61, 1_999_960));
        // Asked about a time before it began, it says where it began.
        assert_eq!(clock.sample(5), (1, 1_000_000));
    }

    #[test]
    fn a_frame_early_in_a_period_is_that_msc_and_moves_its_time() {
        let mut clock = FrameClock::new(0, HZ60);
        clock.frame(16_666 + 3_000);
        assert_eq!(clock.sample(19_666), (2, 19_666));
        // The next one is a period after the frame, not after the tick.
        assert_eq!(clock.sample(19_666 + 16_665), (2, 19_666));
        assert_eq!(clock.sample(19_666 + 16_666), (3, 36_332));
    }

    #[test]
    fn a_frame_late_in_a_period_begins_the_next_msc() {
        let mut clock = FrameClock::new(0, HZ60);
        clock.frame(16_666 + 12_000);
        assert_eq!(clock.sample(28_666), (3, 28_666));
    }

    #[test]
    fn frames_of_several_windows_in_one_refresh_count_once() {
        let mut clock = FrameClock::new(0, HZ60);
        for at in [16_000, 16_100, 16_200] {
            clock.frame(at);
        }
        assert_eq!(clock.sample(16_200).0, 2);
        for at in [32_700, 32_750] {
            clock.frame(at);
        }
        assert_eq!(clock.sample(32_750).0, 3);
    }

    #[test]
    fn frames_at_the_refresh_count_one_msc_each() {
        // The compositor's refresh is not quite the rate it gave: the
        // clock follows the frames, one MSC each, and never skips or
        // repeats one.
        let mut clock = FrameClock::new(0, HZ60);
        let mut last = clock.sample(0);
        for frame in 1..=600_u64 {
            let at = frame * 16_700;
            clock.frame(at);
            let now = clock.sample(at);
            assert_eq!(now.0, last.0 + 1, "frame {frame}");
            assert!(now.1 > last.1);
            last = now;
        }
    }

    #[test]
    fn neither_msc_nor_ust_goes_back() {
        let mut clock = FrameClock::new(0, HZ60);
        let mut last = (0, 0);
        // Ticks, frames at every phase, a stall and a change of rate.
        let mut now = 0;
        for step in 0..2_000_u64 {
            now += [1_000, 7_919, 16_666, 250_000][(step % 4) as usize] / (1 + step % 3);
            match step % 5 {
                0 | 3 => clock.frame(now),
                4 if step % 400 == 4 => {
                    clock.set_refresh(now, [HZ60, 144_000][(step / 400 % 2) as usize])
                }
                _ => {}
            }
            let sample = clock.sample(now);
            assert!(sample.0 >= last.0, "msc went back at step {step}");
            assert!(sample.1 >= last.1, "ust went back at step {step}");
            assert!(sample.1 <= now);
            last = sample;
        }
    }

    #[test]
    fn a_change_of_rate_keeps_the_count() {
        let mut clock = FrameClock::new(0, HZ60);
        clock.set_refresh(50_000, 120_000);
        assert_eq!(clock.period(), 8_333);
        assert_eq!(clock.sample(50_000), (4, 49_998));
        assert_eq!(clock.sample(49_998 + 8_332), (4, 49_998));
        assert_eq!(clock.sample(49_998 + 8_333), (5, 58_331));
    }

    #[test]
    fn an_armed_wake_is_the_next_msc_once_until_it_is_spent() {
        let mut clock = FrameClock::new(0, HZ60);
        assert_eq!(clock.wake(), None);
        assert!(clock.arm(5_000));
        assert_eq!(clock.wake(), Some(16_666));
        assert!(!clock.arm(6_000), "armed already");
        clock.expire(16_000);
        assert_eq!(clock.wake(), Some(16_666), "not due yet");
        clock.expire(16_700);
        assert_eq!(clock.wake(), None);
        assert!(clock.arm(16_700));
        assert_eq!(clock.wake(), Some(33_332));
        // At the wake the MSC it was armed for has begun.
        assert_eq!(clock.sample(33_332).0, 3);
    }

    #[test]
    fn a_frame_moves_an_armed_wake_to_its_phase() {
        let mut clock = FrameClock::new(0, HZ60);
        assert!(clock.arm(1_000));
        clock.frame(4_000);
        assert_eq!(clock.wake(), Some(20_666));
        assert_eq!(clock.sample(20_666).0, 2);
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_us();
        let second = monotonic_us();
        assert!(first > 0);
        assert!(second >= first);
    }
}
