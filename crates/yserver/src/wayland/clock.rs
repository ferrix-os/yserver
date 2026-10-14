//! The Present clock of the rootless Wayland backend: the `(msc, ust)` the
//! root's one RandR output, `WAYLAND-1`, counts its frames by.
//!
//! A server that owns a DRM card counts the kernel's vblanks; a client of a
//! Wayland compositor has none to count. Xorg's `present_fake` gives a
//! screen without a CRTC a timer at its refresh instead, and this clock is
//! that timer: one MSC each refresh period of the compositor's screen
//! (`1e12 / refresh` ns, the rate its `wl_output` gave), counted from when
//! the link came up, whether anything is drawn or not. A
//! `PresentNotifyMSC` for a later MSC always comes due, and every MSC is
//! the same length, so a client that paces itself by the MSC measures the
//! screen's refresh and nothing else.
//!
//! The compositor's frame callbacks do not move it. A callback comes when
//! the compositor drew a frame, and a compositor that draws on demand --
//! hyprix, which draws when a commit comes and its pace allows -- draws
//! when this server's commit comes, which is after this clock's MSC began
//! by the time the server took to read the window back and hand it over.
//! Taking a callback's time as the MSC's moved every MSC that later, and
//! with a client drawing each one the clock ran at a refresh plus that
//! latency (15.7 ms at hyprix's 13.33): one refresh while idle, another
//! while drawn. Kept to its own period, the clock leads and the
//! compositor's frames follow it, one each MSC.
//!
//! The UST is `CLOCK_MONOTONIC` in microseconds, as the kernel's vblank
//! timestamps are; the period is kept in nanoseconds, so that a rate whose
//! period is not a whole microsecond (74.992 Hz, 13 334.7 µs) does not
//! drift from it. Neither number ever goes back: a change of rate keeps
//! the MSC and its time so far.
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
    /// The length of an MSC, in nanoseconds; at least a microsecond.
    period_ns: u64,
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
            period_ns: period_of(refresh_mhz),
            base_msc: 1,
            base_ust: now,
            wake: None,
        }
    }

    /// The length of an MSC, in nanoseconds.
    pub(crate) fn period_ns(&self) -> u64 {
        self.period_ns
    }

    /// The MSC `now` falls in, and the time it began.
    pub(crate) fn sample(&self, now: u64) -> (u64, u64) {
        let elapsed = self.periods_to(now);
        (self.base_msc.wrapping_add(elapsed), self.start_of(elapsed))
    }

    /// The whole periods from the base to `now`: the first microsecond an
    /// MSC is in is the one [`FrameClock::start_of`] gives it.
    fn periods_to(&self, now: u64) -> u64 {
        now.saturating_sub(self.base_ust).saturating_mul(1_000) / self.period_ns
    }

    /// When the MSC `periods` after the base begins: the first whole
    /// microsecond at or after it, so that [`FrameClock::sample`] at that
    /// time is that MSC and a microsecond before it the one before.
    fn start_of(&self, periods: u64) -> u64 {
        self.base_ust + periods.saturating_mul(self.period_ns).div_ceil(1_000)
    }

    /// The compositor's screen refreshes at `refresh_mhz` millihertz from
    /// `now` on (0 for the default). The MSC and its time so far stay.
    pub(crate) fn set_refresh(&mut self, now: u64, refresh_mhz: u64) {
        let period_ns = period_of(refresh_mhz);
        if period_ns == self.period_ns {
            return;
        }
        let (msc, ust) = self.sample(now);
        self.base_msc = msc;
        self.base_ust = ust;
        self.period_ns = period_ns;
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
        self.start_of(self.periods_to(now) + 1)
    }
}

/// An MSC's length in nanoseconds at `refresh_mhz` millihertz.
fn period_of(refresh_mhz: u64) -> u64 {
    let refresh_mhz = if refresh_mhz == 0 {
        DEFAULT_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    // 1e9 ns per second, 1e3 mHz per Hz.
    (1_000_000_000_000 / refresh_mhz).max(1_000)
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

    /// hyprix's screen under QEMU: QEMU's 75 Hz EDID mode, whose pixel
    /// clock in whole kHz gives 74.992 Hz.
    const HZ75: u64 = 74_992;

    #[test]
    fn the_period_is_the_refresh_in_nanoseconds() {
        assert_eq!(FrameClock::new(0, HZ60).period_ns(), 16_666_666);
        assert_eq!(FrameClock::new(0, 59_951).period_ns(), 16_680_288);
        assert_eq!(FrameClock::new(0, HZ75).period_ns(), 13_334_755);
        assert_eq!(FrameClock::new(0, 144_000).period_ns(), 6_944_444);
        assert_eq!(
            FrameClock::new(0, 0).period_ns(),
            16_666_666,
            "no rate is 60 Hz"
        );
    }

    #[test]
    fn the_clock_counts_one_msc_a_period() {
        let clock = FrameClock::new(1_000_000, HZ60);
        assert_eq!(clock.sample(1_000_000), (1, 1_000_000));
        assert_eq!(clock.sample(1_016_666), (1, 1_000_000));
        assert_eq!(clock.sample(1_016_667), (2, 1_016_667));
        // A second later: sixty more, on the dot.
        assert_eq!(clock.sample(2_000_000), (61, 2_000_000));
        // Asked about a time before it began, it says where it began.
        assert_eq!(clock.sample(5), (1, 1_000_000));
    }

    #[test]
    fn every_msc_is_one_period_and_the_count_does_not_drift() {
        // An hour at 74.992 Hz, in microsecond steps around each MSC's
        // start: each begins where the period says, to the microsecond, and
        // is the MSC it should be.
        let start = 7_000_000;
        let clock = FrameClock::new(start, HZ75);
        let period = clock.period_ns();
        let mut last = clock.sample(start);
        for k in 1..=(3_600 * 75_u64) {
            let exact = start * 1_000 + k * period;
            let ust = exact.div_ceil(1_000);
            assert_eq!(clock.sample(ust - 1).0, last.0, "msc {k} came early");
            let now = clock.sample(ust);
            assert_eq!(now, (1 + k, ust), "msc {k}");
            let step = now.1 - last.1;
            assert!(
                (13_334..=13_335).contains(&step),
                "msc {k} was {step} us after the last"
            );
            last = now;
        }
    }

    #[test]
    fn neither_msc_nor_ust_goes_back() {
        let mut clock = FrameClock::new(0, HZ60);
        let mut last = (0, 0);
        // Ticks at every phase, a stall and a change of rate.
        let mut now = 0;
        for step in 0..2_000_u64 {
            now += [1_000, 7_919, 16_666, 250_000][(step % 4) as usize] / (1 + step % 3);
            if step % 400 == 4 {
                clock.set_refresh(now, [HZ60, 144_000, HZ75][(step / 400 % 3) as usize]);
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
        assert_eq!(clock.period_ns(), 8_333_333);
        assert_eq!(clock.sample(50_000), (4, 50_000));
        assert_eq!(clock.sample(50_000 + 8_333), (4, 50_000));
        assert_eq!(clock.sample(50_000 + 8_334), (5, 58_334));
        // The same rate again changes nothing.
        clock.set_refresh(60_000, 120_000);
        assert_eq!(clock.sample(60_000), (5, 58_334));
    }

    #[test]
    fn an_armed_wake_is_the_next_msc_once_until_it_is_spent() {
        let mut clock = FrameClock::new(0, HZ60);
        assert_eq!(clock.wake(), None);
        assert!(clock.arm(5_000));
        assert_eq!(clock.wake(), Some(16_667));
        assert!(!clock.arm(6_000), "armed already");
        clock.expire(16_000);
        assert_eq!(clock.wake(), Some(16_667), "not due yet");
        clock.expire(16_700);
        assert_eq!(clock.wake(), None);
        assert!(clock.arm(16_700));
        assert_eq!(clock.wake(), Some(33_334));
        // At the wake the MSC it was armed for has begun.
        assert_eq!(clock.sample(33_334).0, 3);
        assert_eq!(clock.sample(33_333).0, 2);
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_us();
        let second = monotonic_us();
        assert!(first > 0);
        assert!(second >= first);
    }
}
