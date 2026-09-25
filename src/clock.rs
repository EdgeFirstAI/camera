// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Conversion of V4L2 capture timestamps from CLOCK_MONOTONIC to
//! CLOCK_REALTIME for ROS 2 header stamps and Zenoh sample timestamps.
//!
//! The offset between the two clocks is measured at every conversion
//! rather than cached. Slewing moves both clocks together, so the offset
//! only changes when the wall clock is stepped (NTP or GNSS sync on a unit
//! without a working RTC, or `date -s`), and a step can arrive at any time
//! during operation or never. Measuring per conversion follows a step on
//! the next frame without a restart.
//!
//! The two clocks cannot be read atomically, so each measurement brackets
//! the monotonic read between two realtime reads and uses the midpoint of
//! the realtime pair. Preemption between the reads widens the bracket and
//! adds an error up to its width, so a wide bracket is measured once more
//! and the tighter of the two is kept. All reads are vDSO calls.

use edgefirst_schemas::builtin_interfaces::Time;
use std::io;
use tracing::{info, warn};
use unix_ts::Timestamp;

const NSEC_PER_SEC: i128 = 1_000_000_000;

/// Bracket width above which the offset is measured a second time.
const MAX_BRACKET_NS: i128 = 10_000;

/// Offset change reported as a clock step.
const STEP_REPORT_NS: i128 = NSEC_PER_SEC;

/// Stamp published when the converted time exceeds the ROS 2 `Time` range
/// (`i32` seconds, 2038-01-19T03:14:07Z).
pub(crate) const SATURATED_TIME: Time = Time {
    sec: i32::MAX,
    nanosec: 999_999_999,
};

/// Source of clock readings, in nanoseconds.
pub(crate) trait ClockSource {
    fn realtime_ns(&mut self) -> io::Result<i128>;
    fn monotonic_ns(&mut self) -> io::Result<i128>;
}

/// The host clocks via `clock_gettime`.
pub(crate) struct SystemClocks;

impl SystemClocks {
    fn read(clock: libc::clockid_t) -> io::Result<i128> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec for the duration of
        // the call.
        if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ts.tv_sec as i128 * NSEC_PER_SEC + ts.tv_nsec as i128)
    }
}

impl ClockSource for SystemClocks {
    fn realtime_ns(&mut self) -> io::Result<i128> {
        Self::read(libc::CLOCK_REALTIME)
    }

    fn monotonic_ns(&mut self) -> io::Result<i128> {
        Self::read(libc::CLOCK_MONOTONIC)
    }
}

/// Converts CLOCK_MONOTONIC instants to CLOCK_REALTIME, following steps of
/// the wall clock as they happen.
pub(crate) struct RealtimeClock<S = SystemClocks> {
    source: S,
    last_offset_ns: Option<i128>,
}

impl RealtimeClock<SystemClocks> {
    pub(crate) fn new() -> Self {
        Self::with_source(SystemClocks)
    }
}

impl<S: ClockSource> RealtimeClock<S> {
    pub(crate) fn with_source(source: S) -> Self {
        Self {
            source,
            last_offset_ns: None,
        }
    }

    /// Converts a V4L2 CLOCK_MONOTONIC capture timestamp to CLOCK_REALTIME.
    ///
    /// Times before the Unix epoch clamp to the epoch and times past the
    /// ROS 2 `Time` range saturate to [`SATURATED_TIME`], so the header
    /// stamp and the Zenoh sample timestamp derived from it always agree.
    pub(crate) fn convert(&mut self, ts: &Timestamp) -> io::Result<Time> {
        let mono_ns = ts.seconds() as i128 * NSEC_PER_SEC + ts.subsec(9) as i128;
        let offset_ns = self.offset_ns()?;
        Ok(time_from_ns(mono_ns + offset_ns))
    }

    /// Measures CLOCK_REALTIME - CLOCK_MONOTONIC and logs a step when it
    /// differs from the previous measurement by more than a second.
    fn offset_ns(&mut self) -> io::Result<i128> {
        let (mut offset, bracket) = self.sample()?;
        if bracket > MAX_BRACKET_NS {
            let (retry, retry_bracket) = self.sample()?;
            if retry_bracket < bracket {
                offset = retry;
            }
        }

        match self.last_offset_ns {
            None => info!(
                "CLOCK_REALTIME - CLOCK_MONOTONIC = {:.9} s",
                offset as f64 / NSEC_PER_SEC as f64
            ),
            Some(prev) if (offset - prev).abs() > STEP_REPORT_NS => info!(
                "CLOCK_REALTIME stepped by {:+.3} s; capture stamps follow the new clock",
                (offset - prev) as f64 / NSEC_PER_SEC as f64
            ),
            Some(_) => {}
        }
        self.last_offset_ns = Some(offset);
        Ok(offset)
    }

    /// One bracketed measurement: `(offset, bracket width)`.
    fn sample(&mut self) -> io::Result<(i128, i128)> {
        let before = self.source.realtime_ns()?;
        let mono = self.source.monotonic_ns()?;
        let after = self.source.realtime_ns()?;
        let bracket = after - before;
        Ok((before + bracket / 2 - mono, bracket.abs()))
    }
}

/// Current CLOCK_REALTIME as a ROS 2 `Time`, clamped the same way as
/// [`RealtimeClock::convert`].
pub(crate) fn now() -> io::Result<Time> {
    SystemClocks.realtime_ns().map(time_from_ns)
}

fn time_from_ns(ns: i128) -> Time {
    if ns < 0 {
        return Time { sec: 0, nanosec: 0 };
    }
    let sec = ns / NSEC_PER_SEC;
    if sec > i32::MAX as i128 {
        warn!("Timestamp overflow: converted time exceeds i32 range (Y2038), saturating");
        return SATURATED_TIME;
    }
    Time {
        sec: sec as i32,
        nanosec: (ns % NSEC_PER_SEC) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Replays scripted `(realtime, monotonic)` readings in call order.
    struct Script {
        realtime: VecDeque<i128>,
        monotonic: VecDeque<i128>,
    }

    impl Script {
        /// One entry per measurement: `(before, mono, after)`.
        fn new(samples: &[(i128, i128, i128)]) -> Self {
            let mut realtime = VecDeque::new();
            let mut monotonic = VecDeque::new();
            for &(before, mono, after) in samples {
                realtime.push_back(before);
                monotonic.push_back(mono);
                realtime.push_back(after);
            }
            Script {
                realtime,
                monotonic,
            }
        }
    }

    impl ClockSource for Script {
        fn realtime_ns(&mut self) -> io::Result<i128> {
            Ok(self.realtime.pop_front().expect("script exhausted"))
        }

        fn monotonic_ns(&mut self) -> io::Result<i128> {
            Ok(self.monotonic.pop_front().expect("script exhausted"))
        }
    }

    const S: i128 = NSEC_PER_SEC;

    fn mono(sec: i64, nsec: u32) -> Timestamp {
        Timestamp::new(sec, nsec)
    }

    #[test]
    fn converts_with_bracket_midpoint() {
        // realtime 1000 s + [0, 2 us] around monotonic 10 s: offset is
        // 990 s + 1 us.
        let mut clock =
            RealtimeClock::with_source(Script::new(&[(1000 * S, 10 * S, 1000 * S + 2_000)]));
        let t = clock.convert(&mono(5, 500)).unwrap();
        assert_eq!(
            t,
            Time {
                sec: 995,
                nanosec: 1_500
            }
        );
    }

    #[test]
    fn follows_forward_and_backward_steps() {
        let mut clock = RealtimeClock::with_source(Script::new(&[
            (1000 * S, 10 * S, 1000 * S),
            // Wall clock stepped forward by 475 days.
            (1000 * S + 41_054_973 * S, 11 * S, 1000 * S + 41_054_973 * S),
            // Then back by 30 s.
            (970 * S + 41_054_973 * S, 12 * S, 970 * S + 41_054_973 * S),
        ]));
        let ts = mono(10, 0);
        assert_eq!(clock.convert(&ts).unwrap().sec, 1000);
        assert_eq!(clock.convert(&ts).unwrap().sec, 1000 + 41_054_972);
        assert_eq!(clock.convert(&ts).unwrap().sec, 970 + 41_054_971);
    }

    #[test]
    fn wide_bracket_is_remeasured_and_tighter_kept() {
        // First bracket 50 us (preempted) with midpoint +25 us; retry is
        // exact.
        let mut clock = RealtimeClock::with_source(Script::new(&[
            (1000 * S, 10 * S, 1000 * S + 50_000),
            (1000 * S, 10 * S, 1000 * S),
        ]));
        let t = clock.convert(&mono(10, 0)).unwrap();
        assert_eq!(
            t,
            Time {
                sec: 1000,
                nanosec: 0
            }
        );
    }

    #[test]
    fn wide_retry_keeps_first_when_tighter() {
        let mut clock = RealtimeClock::with_source(Script::new(&[
            (1000 * S, 10 * S, 1000 * S + 20_000),
            (1000 * S, 10 * S, 1000 * S + 80_000),
        ]));
        let t = clock.convert(&mono(10, 0)).unwrap();
        assert_eq!(
            t,
            Time {
                sec: 1000,
                nanosec: 10_000
            }
        );
    }

    #[test]
    fn narrow_bracket_is_measured_once() {
        // A second measurement would exhaust the script and panic.
        let mut clock =
            RealtimeClock::with_source(Script::new(&[(1000 * S, 10 * S, 1000 * S + 10_000)]));
        clock.convert(&mono(10, 0)).unwrap();
    }

    #[test]
    fn negative_offset_normalizes_nanoseconds() {
        // Realtime behind monotonic by 0.25 s (clock set to a pre-boot
        // time on a long-running unit).
        let mut clock = RealtimeClock::with_source(Script::new(&[(S, S + S / 4, S)]));
        let t = clock.convert(&mono(2, 100)).unwrap();
        assert_eq!(
            t,
            Time {
                sec: 1,
                nanosec: 750_000_100
            }
        );
    }

    #[test]
    fn pre_epoch_clamps_to_epoch() {
        let mut clock = RealtimeClock::with_source(Script::new(&[(0, 100 * S, 0)]));
        let t = clock.convert(&mono(10, 0)).unwrap();
        assert_eq!(t, Time { sec: 0, nanosec: 0 });
    }

    #[test]
    fn past_y2038_saturates() {
        let big = (i32::MAX as i128 + 10) * S;
        let mut clock = RealtimeClock::with_source(Script::new(&[(big, 0, big)]));
        let t = clock.convert(&mono(0, 0)).unwrap();
        assert_eq!(t, SATURATED_TIME);
    }

    #[test]
    fn system_clocks_convert_to_now() {
        let mut clock = RealtimeClock::new();
        let mut src = SystemClocks;
        let mono_now = src.monotonic_ns().unwrap();
        let ts = Timestamp::new((mono_now / S) as i64, (mono_now % S) as u32);
        let converted = clock.convert(&ts).unwrap();
        let real_now = src.realtime_ns().unwrap();
        let converted_ns = converted.sec as i128 * S + converted.nanosec as i128;
        assert!((real_now - converted_ns).abs() < 10_000_000);
    }
}
