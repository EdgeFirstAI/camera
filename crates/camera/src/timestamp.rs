// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Capture timestamps and their conversion to wall-clock acquisition time.
//!
//! Backends report the capture instant in whatever clock the driver uses,
//! together with where in the frame the instant was taken. Under the
//! EdgeFirst timestamp contract the acquisition time of a frame is that
//! instant expressed in host `CLOCK_REALTIME`; [`RealtimeClock`] performs
//! the conversion.
//!
//! The offset between the capture clock and `CLOCK_REALTIME` is measured at
//! every conversion rather than cached. Slewing moves both clocks together,
//! so the offset only changes when the wall clock is stepped (NTP or GNSS
//! sync on a unit without a working RTC, or `date -s`), which can happen at
//! any time during operation. The two clocks cannot be read atomically, so
//! each measurement brackets the capture-clock read between two realtime
//! reads and uses the midpoint of the realtime pair. Preemption between the
//! reads widens the bracket, so a wide bracket is measured once more and
//! the tighter of the two is kept. All reads are vDSO calls.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::info;

use crate::{Error, ErrorKind, Result};

const NSEC_PER_SEC: i128 = 1_000_000_000;

/// Bracket width above which the offset is measured a second time.
const MAX_BRACKET_NS: i128 = 10_000;

/// Offset change reported as a wall-clock step.
const STEP_REPORT_NS: i128 = NSEC_PER_SEC;

/// Clock in which a backend reports the capture instant.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureClock {
    /// Linux `CLOCK_MONOTONIC`, used by V4L2 drivers and, in practice, by
    /// libcamera's `SensorTimestamp`.
    Monotonic,
    /// Linux `CLOCK_BOOTTIME`, which keeps counting through suspend.
    Boottime,
    /// Wall-clock time, already in the acquisition-time domain.
    Realtime,
    /// The backend did not say; no conversion is possible.
    Unknown,
}

/// Where within the frame the capture instant was taken.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimestampSource {
    /// When the last line was received (the V4L2 default).
    EndOfFrame,
    /// When the first line was received (RPi CFE under libcamera).
    StartOfFrame,
    /// When exposure of the first line began (`V4L2_BUF_FLAG_TSTAMP_SRC_SOE`).
    StartOfExposure,
    /// The backend did not say.
    Unknown,
}

/// A capture instant in the clock the backend reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp {
    /// Clock the instant is expressed in.
    pub clock: CaptureClock,
    /// Where within the frame the instant was taken.
    pub source: TimestampSource,
    /// Nanoseconds since the clock's epoch.
    pub nanos: i64,
}

/// Source of clock readings, in nanoseconds.
pub(crate) trait ClockReader {
    fn realtime_ns(&mut self) -> std::io::Result<i128>;
    fn capture_ns(&mut self, clock: CaptureClock) -> std::io::Result<i128>;
}

/// The host clocks.
#[derive(Debug, Default)]
pub(crate) struct SystemClocks;

#[cfg(unix)]
fn clock_ns(clock: libc::clockid_t) -> std::io::Result<i128> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ts.tv_sec as i128 * NSEC_PER_SEC + ts.tv_nsec as i128)
}

impl ClockReader for SystemClocks {
    fn realtime_ns(&mut self) -> std::io::Result<i128> {
        let now = SystemTime::now();
        Ok(match now.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        })
    }

    fn capture_ns(&mut self, clock: CaptureClock) -> std::io::Result<i128> {
        match clock {
            #[cfg(unix)]
            CaptureClock::Monotonic => clock_ns(libc::CLOCK_MONOTONIC),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            CaptureClock::Boottime => clock_ns(libc::CLOCK_BOOTTIME),
            CaptureClock::Realtime => self.realtime_ns(),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("capture clock {clock:?} is not available on this platform"),
            )),
        }
    }
}

/// Reads the current instant of `clock` on this host.
#[cfg_attr(
    not(feature = "mock"),
    expect(
        dead_code,
        reason = "used by backends; only the mock backend exists yet"
    )
)]
pub(crate) fn now(clock: CaptureClock) -> std::io::Result<i64> {
    SystemClocks.capture_ns(clock).map(|ns| ns as i64)
}

/// Converts capture timestamps to wall-clock acquisition time, following
/// steps of the wall clock as they happen.
///
/// The offset is measured at every conversion and never cached; a change of
/// more than one second (a wall-clock step) is logged once at INFO.
///
/// # Examples
///
/// ```
/// use edgefirst_camera::{CaptureClock, RealtimeClock, Timestamp, TimestampSource};
/// use std::time::SystemTime;
///
/// let mut clock = RealtimeClock::new();
/// let now = SystemTime::now()
///     .duration_since(SystemTime::UNIX_EPOCH)
///     .unwrap()
///     .as_nanos() as i64;
/// let ts = Timestamp { clock: CaptureClock::Realtime, source: TimestampSource::Unknown, nanos: now };
/// assert!(clock.to_realtime(ts).is_ok());
/// ```
#[derive(Debug)]
pub struct RealtimeClock {
    inner: Converter<SystemClocks>,
}

impl RealtimeClock {
    /// Creates a converter over the host clocks.
    pub fn new() -> Self {
        Self {
            inner: Converter::new(SystemClocks),
        }
    }

    /// Converts `ts` to wall-clock time.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidConfig`] for [`CaptureClock::Unknown`],
    /// [`ErrorKind::Backend`] when the capture clock cannot be read on this
    /// platform, and [`ErrorKind::Io`] when a clock read fails.
    pub fn to_realtime(&mut self, ts: Timestamp) -> Result<SystemTime> {
        let ns = self.inner.realtime_ns(ts)?;
        system_time_from_ns(ns)
    }
}

impl Default for RealtimeClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a signed nanosecond count since the Unix epoch to `SystemTime`.
pub(crate) fn system_time_from_ns(ns: i128) -> Result<SystemTime> {
    let magnitude = Duration::from_nanos(u64::try_from(ns.unsigned_abs()).map_err(|_| {
        Error::new(
            ErrorKind::InvalidConfig,
            "timestamp outside the SystemTime range",
        )
    })?);
    let t = if ns >= 0 {
        UNIX_EPOCH.checked_add(magnitude)
    } else {
        UNIX_EPOCH.checked_sub(magnitude)
    };
    t.ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidConfig,
            "timestamp outside the SystemTime range",
        )
    })
}

/// Bracketed per-conversion offset measurement over a [`ClockReader`].
#[derive(Debug)]
pub(crate) struct Converter<R> {
    reader: R,
    last_offset_ns: [Option<i128>; 2],
}

impl<R: ClockReader> Converter<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            last_offset_ns: [None; 2],
        }
    }

    /// Converts `ts` to signed nanoseconds since the Unix epoch.
    pub(crate) fn realtime_ns(&mut self, ts: Timestamp) -> Result<i128> {
        let slot = match ts.clock {
            CaptureClock::Realtime => return Ok(ts.nanos as i128),
            CaptureClock::Monotonic => 0,
            CaptureClock::Boottime => 1,
            CaptureClock::Unknown => {
                return Err(Error::new(
                    ErrorKind::InvalidConfig,
                    "capture clock unknown; cannot convert to wall-clock time",
                ))
            }
        };
        let offset = self.offset_ns(ts.clock, slot)?;
        Ok(ts.nanos as i128 + offset)
    }

    fn offset_ns(&mut self, clock: CaptureClock, slot: usize) -> Result<i128> {
        let (mut offset, bracket) = self.sample(clock)?;
        if bracket > MAX_BRACKET_NS {
            let (retry, retry_bracket) = self.sample(clock)?;
            if retry_bracket < bracket {
                offset = retry;
            }
        }
        match self.last_offset_ns[slot] {
            None => info!(
                "CLOCK_REALTIME - {clock:?} = {:.9} s",
                offset as f64 / NSEC_PER_SEC as f64
            ),
            Some(prev) if (offset - prev).abs() > STEP_REPORT_NS => info!(
                "CLOCK_REALTIME stepped by {:+.3} s; capture stamps follow the new clock",
                (offset - prev) as f64 / NSEC_PER_SEC as f64
            ),
            Some(_) => {}
        }
        self.last_offset_ns[slot] = Some(offset);
        Ok(offset)
    }

    /// One bracketed measurement: `(offset, bracket width)`.
    fn sample(&mut self, clock: CaptureClock) -> Result<(i128, i128)> {
        let map = |e: std::io::Error| {
            let kind = if e.kind() == std::io::ErrorKind::Unsupported {
                ErrorKind::Backend
            } else {
                ErrorKind::Io
            };
            Error::new(kind, e.to_string()).with_source(e)
        };
        let before = self.reader.realtime_ns().map_err(map)?;
        let capture = self.reader.capture_ns(clock).map_err(map)?;
        let after = self.reader.realtime_ns().map_err(map)?;
        let bracket = after - before;
        Ok((before + bracket / 2 - capture, bracket.abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const S: i128 = NSEC_PER_SEC;

    /// Replays scripted `(realtime, capture)` readings in call order.
    struct Script {
        realtime: VecDeque<i128>,
        capture: VecDeque<i128>,
    }

    impl Script {
        /// One entry per measurement: `(before, capture, after)`.
        fn new(samples: &[(i128, i128, i128)]) -> Self {
            let mut realtime = VecDeque::new();
            let mut capture = VecDeque::new();
            for &(before, cap, after) in samples {
                realtime.push_back(before);
                capture.push_back(cap);
                realtime.push_back(after);
            }
            Script { realtime, capture }
        }
    }

    impl ClockReader for Script {
        fn realtime_ns(&mut self) -> std::io::Result<i128> {
            Ok(self.realtime.pop_front().expect("script exhausted"))
        }

        fn capture_ns(&mut self, _clock: CaptureClock) -> std::io::Result<i128> {
            Ok(self.capture.pop_front().expect("script exhausted"))
        }
    }

    fn mono(sec: i64, nsec: i64) -> Timestamp {
        Timestamp {
            clock: CaptureClock::Monotonic,
            source: TimestampSource::EndOfFrame,
            nanos: sec * 1_000_000_000 + nsec,
        }
    }

    #[test]
    fn converts_with_bracket_midpoint() {
        // realtime 1000 s + [0, 2 us] around monotonic 10 s: offset is
        // 990 s + 1 us.
        let mut c = Converter::new(Script::new(&[(1000 * S, 10 * S, 1000 * S + 2_000)]));
        assert_eq!(c.realtime_ns(mono(5, 500)).unwrap(), 995 * S + 1_500);
    }

    #[test]
    fn follows_forward_and_backward_steps() {
        let mut c = Converter::new(Script::new(&[
            (1000 * S, 10 * S, 1000 * S),
            // Wall clock stepped forward by 475 days.
            (1000 * S + 41_054_973 * S, 11 * S, 1000 * S + 41_054_973 * S),
            // Then back by 30 s.
            (970 * S + 41_054_973 * S, 12 * S, 970 * S + 41_054_973 * S),
        ]));
        let ts = mono(10, 0);
        assert_eq!(c.realtime_ns(ts).unwrap() / S, 1000);
        assert_eq!(c.realtime_ns(ts).unwrap() / S, 1000 + 41_054_972);
        assert_eq!(c.realtime_ns(ts).unwrap() / S, 970 + 41_054_971);
    }

    #[test]
    fn wide_bracket_is_remeasured_and_tighter_kept() {
        let mut c = Converter::new(Script::new(&[
            (1000 * S, 10 * S, 1000 * S + 50_000),
            (1000 * S, 10 * S, 1000 * S),
        ]));
        assert_eq!(c.realtime_ns(mono(10, 0)).unwrap(), 1000 * S);
    }

    #[test]
    fn wide_retry_keeps_first_when_tighter() {
        let mut c = Converter::new(Script::new(&[
            (1000 * S, 10 * S, 1000 * S + 20_000),
            (1000 * S, 10 * S, 1000 * S + 80_000),
        ]));
        assert_eq!(c.realtime_ns(mono(10, 0)).unwrap(), 1000 * S + 10_000);
    }

    #[test]
    fn narrow_bracket_is_measured_once() {
        // A second measurement would exhaust the script and panic.
        let mut c = Converter::new(Script::new(&[(1000 * S, 10 * S, 1000 * S + 10_000)]));
        c.realtime_ns(mono(10, 0)).unwrap();
    }

    #[test]
    fn negative_offset_is_exact() {
        // Realtime behind monotonic by 0.25 s.
        let mut c = Converter::new(Script::new(&[(S, S + S / 4, S)]));
        assert_eq!(c.realtime_ns(mono(2, 100)).unwrap(), S + 750_000_100);
    }

    #[test]
    fn pre_epoch_is_representable() {
        let mut c = Converter::new(Script::new(&[(0, 100 * S, 0)]));
        let ns = c.realtime_ns(mono(10, 0)).unwrap();
        assert_eq!(ns, -90 * S);
        assert!(system_time_from_ns(ns).unwrap() < UNIX_EPOCH);
    }

    #[test]
    fn realtime_passes_through_and_unknown_fails() {
        let mut c = Converter::new(Script::new(&[]));
        let rt = Timestamp {
            clock: CaptureClock::Realtime,
            source: TimestampSource::Unknown,
            nanos: 42,
        };
        assert_eq!(c.realtime_ns(rt).unwrap(), 42);
        let unknown = Timestamp {
            clock: CaptureClock::Unknown,
            ..rt
        };
        assert_eq!(
            c.realtime_ns(unknown).unwrap_err().kind(),
            ErrorKind::InvalidConfig
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_clocks_convert_to_now() {
        let mut clock = RealtimeClock::new();
        let mut src = SystemClocks;
        let real_before = src.realtime_ns().unwrap();
        let mono_now = src.capture_ns(CaptureClock::Monotonic).unwrap();
        let real_after = src.realtime_ns().unwrap();
        let converted = clock.to_realtime(mono(0, mono_now as i64)).unwrap();
        let converted_ns = converted.duration_since(UNIX_EPOCH).unwrap().as_nanos() as i128;
        // The monotonic read happened between the two realtime reads, so its
        // conversion must land in that window regardless of scheduling delay.
        let slack = 1_000_000;
        assert!(
            converted_ns >= real_before - slack,
            "{converted_ns} < {real_before}"
        );
        assert!(
            converted_ns <= real_after + slack,
            "{converted_ns} > {real_after}"
        );
    }
}
