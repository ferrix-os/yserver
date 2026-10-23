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
//! Except while a client's own buffer waits to be shown. A whole-window
//! present handed to the compositor as the client's buffer (`super::direct`)
//! is a flip, and Xwayland lets the MSC of a window with a flip pending move
//! only when the compositor's frame callback for it comes
//! (`xwl_present_frame_callback` -> `xwl_present_msc_bump` in
//! `hw/xwayland/xwayland-present.c`), its 17 ms timer stretched to a second
//! (`TIMER_LEN_FLIP`) in case none ever comes. So does this clock
//! ([`FrameClock::hold`], [`FrameClock::frame_done`]): from the commit until
//! the compositor drew it, the MSC stays, and the callback begins the next.
//! A FIFO swapchain -- Mesa's X11 WSI waits for each present's
//! `PresentCompleteNotify` and asks for the MSC after it -- then presents
//! one frame for each the compositor drew, and none is replaced by the next
//! before it was shown. Timed by the timer alone, a game presenting in
//! pairs (FSR3 frame generation) had two MSCs come due while one frame
//! waited for its callback, and lost the first: 35 presents a second, 19
//! shown.
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

/// How long the clock waits for a held frame's callback before it counts on
/// without it, in microseconds: Xwayland's `TIMER_LEN_FLIP`. A compositor
/// that shows the surface nowhere sends none.
const HOLD_US: u64 = 1_000_000;

/// A frame of a client's own buffer the compositor has not drawn yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    /// The MSC it was committed in, and when that MSC began.
    msc: u64,
    ust: u64,
    /// When it was committed.
    since: u64,
}

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
    /// A committed frame the compositor has not drawn yet: the MSC stays
    /// where it is until it has, or for [`HOLD_US`].
    held: Option<Held>,
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
            held: None,
        }
    }

    /// The length of an MSC, in nanoseconds.
    pub(crate) fn period_ns(&self) -> u64 {
        self.period_ns
    }

    /// The MSC `now` falls in, and the time it began.
    pub(crate) fn sample(&self, now: u64) -> (u64, u64) {
        if let Some(held) = self.held {
            if now < held.since.saturating_add(HOLD_US) {
                return (held.msc, held.ust);
            }
            // No callback came in time: the clock went on without it.
            return self.released(held).sample(now);
        }
        let elapsed = self.periods_to(now);
        (self.base_msc.wrapping_add(elapsed), self.start_of(elapsed))
    }

    /// The clock as it is once `held` ends without a callback: the next
    /// MSC begins when the wait for it ran out.
    fn released(&self, held: Held) -> Self {
        Self {
            base_msc: held.msc.wrapping_add(1),
            base_ust: held.since.saturating_add(HOLD_US),
            held: None,
            ..*self
        }
    }

    /// A hold whose wait ran out by `now` ends, as if the timer had.
    fn settle(&mut self, now: u64) {
        if let Some(held) = self.held
            && now >= held.since.saturating_add(HOLD_US)
        {
            *self = Self {
                wake: self.wake,
                ..self.released(held)
            };
        }
    }

    /// A frame of a client's own buffer was committed at `now`: the MSC
    /// stays until the compositor's callback for it ([`FrameClock::frame_done`]).
    /// A second commit while one is held changes nothing.
    pub(crate) fn hold(&mut self, now: u64) {
        self.settle(now);
        if self.held.is_some() {
            return;
        }
        let (msc, ust) = self.sample(now);
        self.held = Some(Held {
            msc,
            ust,
            since: now,
        });
        // A wake armed for the next MSC by the timer is too early now.
        self.wake = None;
    }

    /// The compositor drew the held frame at `now`: the next MSC begins.
    /// Whether one did (`false` without a hold, or after one that ran out,
    /// which began its next MSC already).
    pub(crate) fn frame_done(&mut self, now: u64) -> bool {
        self.settle(now);
        let Some(held) = self.held.take() else {
            return false;
        };
        self.base_msc = held.msc.wrapping_add(1);
        self.base_ust = now.max(held.ust);
        true
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
        self.settle(now);
        if self.held.is_some() {
            // The period counts from the MSC the callback begins.
            self.period_ns = period_ns;
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
        self.settle(now);
        if self.wake.is_some_and(|wake| wake > now) {
            return false;
        }
        // While a frame is held the next MSC is its callback's, which wakes
        // the loop by itself; the wake is for the callback that never comes.
        self.wake = Some(match self.held {
            Some(held) => held.since.saturating_add(HOLD_US),
            None => self.next_tick(now),
        });
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
    fn a_client_paced_by_the_msc_measures_one_refresh_however_late_it_is_woken() {
        // What moved the clock before it kept its own period: a client
        // waits for the next MSC each frame, the core comes round late by
        // its own latency, the client draws, and the frame it drew comes
        // back from the compositor later still. None of it moves the
        // clock: each MSC the client is told of is the one after the last,
        // one refresh after it, as Xorg's `present_fake` would say, and a
        // stall misses the MSCs it lasted.
        let start = 2_000_000;
        let mut clock = FrameClock::new(start, HZ75);
        let mut now = start;
        let mut last = clock.sample(now);
        for frame in 1..=750_u64 {
            assert!(clock.arm(now), "frame {frame}: nothing was armed");
            let wake = clock.wake().expect("armed");
            // Woken 0 to 3 ms late, or every 250th frame 50 ms late: a
            // stall of 3.75 MSCs, which with the drawing after it still
            // ends inside the MSC it reached.
            let stall = if frame % 250 == 0 { 50_000 } else { 0 };
            let late = if stall > 0 {
                0
            } else {
                (frame * 7_919) % 3_000
            };
            now = wake + late + stall;
            clock.expire(now);
            assert_eq!(clock.wake(), None, "frame {frame}: the wake was spent");
            let told = clock.sample(now);
            let missed = told.0 - last.0 - 1;
            assert_eq!(missed, if stall > 0 { 3 } else { 0 }, "frame {frame}");
            let step = (told.1 - last.1) / (missed + 1);
            assert!(
                (13_334..=13_335).contains(&step),
                "frame {frame}: an MSC of {step} us"
            );
            last = told;
            // Drawing and handing the frame over: 2.4 ms more.
            now += 2_400;
        }
    }

    #[test]
    fn a_held_frame_keeps_the_msc_until_its_callback_begins_the_next() {
        let mut clock = FrameClock::new(0, HZ60);
        // Committed in MSC 2; the timer would be at 5 by 70 ms.
        clock.hold(20_000);
        assert_eq!(clock.sample(20_000), (2, 16_667));
        assert_eq!(clock.sample(70_000), (2, 16_667), "held");
        // The callback at 71 ms begins MSC 3 there, and the timer counts on
        // from it.
        assert!(clock.frame_done(71_000));
        assert_eq!(clock.sample(71_000), (3, 71_000));
        assert_eq!(clock.sample(71_000 + 16_667), (4, 87_667));
        assert!(!clock.frame_done(90_000), "nothing held");
    }

    #[test]
    fn a_wake_while_held_is_for_the_callback_that_never_comes() {
        let mut clock = FrameClock::new(0, HZ60);
        assert!(clock.arm(1_000));
        assert_eq!(clock.wake(), Some(16_667));
        clock.hold(2_000);
        assert_eq!(clock.wake(), None, "the timer's wake is too early");
        assert!(clock.arm(2_000));
        assert_eq!(clock.wake(), Some(2_000 + HOLD_US));
    }

    #[test]
    fn a_hold_runs_out_after_a_second_and_its_late_callback_moves_nothing() {
        let mut clock = FrameClock::new(0, HZ60);
        clock.hold(10_000);
        let out = 10_000 + HOLD_US;
        assert_eq!(clock.sample(out - 1), (1, 0));
        assert_eq!(clock.sample(out), (2, out));
        assert_eq!(clock.sample(out + 16_667), (3, out + 16_667));
        // The callback after it does not count twice, and nothing goes back.
        let before = clock.sample(out + 20_000);
        assert!(!clock.frame_done(out + 20_000));
        assert_eq!(clock.sample(out + 20_000), before);
    }

    #[test]
    fn a_game_presenting_each_msc_shows_one_frame_a_callback() {
        // FIFO: each present asks for the MSC after the last completed one
        // and is executed -- committed, held -- when that MSC begins. With
        // the compositor drawing every 16.7 ms, a little late each time,
        // every MSC is one callback and the count never skips.
        let mut clock = FrameClock::new(0, HZ60);
        let mut now = 5_000;
        let mut last = clock.sample(now).0;
        for frame in 0..600_u64 {
            clock.hold(now);
            // The compositor draws 9 to 15 ms after the commit.
            now += 9_000 + (frame * 7_919) % 6_000;
            assert_eq!(clock.sample(now).0, last, "frame {frame}: moved while held");
            assert!(clock.frame_done(now));
            let msc = clock.sample(now).0;
            assert_eq!(msc, last + 1, "frame {frame}");
            last = msc;
            now += 500;
        }
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_us();
        let second = monotonic_us();
        assert!(first > 0);
        assert!(second >= first);
    }
}
