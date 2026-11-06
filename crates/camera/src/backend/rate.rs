// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! The delivered rate: frames over time, from capture timestamps, over a window
//! of about a second. A stall lowers the window it falls in.

/// How long each measurement lasts, at least.
const WINDOW_NANOS: i64 = 1_000_000_000;

/// Frames at the start of a stream left out of the measurement: the intervals
/// between them can include the sensor starting up.
const START_FRAMES: u32 = 3;

/// Measures the delivered rate.
#[derive(Debug, Default)]
pub(crate) struct RateMeter {
    /// Frames of this start seen, up to `START_FRAMES`.
    started: u32,
    /// Where the window in progress began.
    first_nanos: i64,
    /// Intervals in the window in progress.
    intervals: u64,
    measured: Option<f64>,
}

impl RateMeter {
    /// Forgets the stream, for a stop or a new rate.
    pub(crate) fn restart(&mut self) {
        *self = Self::default();
    }

    /// The rate over the latest whole window since the last restart.
    pub(crate) fn measured(&self) -> Option<f64> {
        self.measured
    }

    /// Takes the capture timestamp of a delivered frame.
    pub(crate) fn frame(&mut self, nanos: i64) {
        if self.started < START_FRAMES {
            self.started += 1;
            self.first_nanos = nanos;
            return;
        }
        self.intervals += 1;
        let span = nanos - self.first_nanos;
        if span >= WINDOW_NANOS {
            self.measured = Some(round_fps(self.intervals as f64 * 1e9 / span as f64));
            self.first_nanos = nanos;
            self.intervals = 0;
        }
    }
}

/// A rate rounded to hundredths, as reported.
pub(crate) fn round_fps(fps: f64) -> f64 {
    (fps * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;
    const AT_30_FPS: i64 = 33_333_333;

    /// Feeds frames at `intervals`, after any before them.
    fn feed(meter: &mut RateMeter, last: &mut Option<i64>, intervals: &[i64]) {
        let mut t = last.unwrap_or_else(|| {
            meter.frame(0);
            0
        });
        for interval in intervals {
            t += interval;
            meter.frame(t);
        }
        *last = Some(t);
    }

    fn measured_after(intervals: &[i64]) -> Option<f64> {
        let mut meter = RateMeter::default();
        feed(&mut meter, &mut None, intervals);
        meter.measured()
    }

    #[test]
    fn nothing_is_measured_until_a_window_has_ended() {
        assert_eq!(measured_after(&[AT_30_FPS; 25]), None);
        assert_eq!(measured_after(&[AT_30_FPS; 40]), Some(30.0));
    }

    #[test]
    fn the_start_of_a_stream_is_left_out() {
        let mut intervals = vec![348 * MS, 200 * MS];
        intervals.extend([AT_30_FPS; 40]);
        assert_eq!(measured_after(&intervals), Some(30.0));
    }

    /// The Brio 101: timestamps quantised to the USB frame, at a true 30 fps.
    #[test]
    fn jittery_timestamps_average_out_in_every_window() {
        let mut meter = RateMeter::default();
        let mut last = None;
        for _ in 0..30 {
            feed(
                &mut meter,
                &mut last,
                &[32 * MS, 32 * MS, 36 * MS].repeat(11),
            );
            let fps = meter.measured().unwrap();
            assert!((fps - 30.0).abs() < 0.3, "{fps}");
        }
    }

    #[test]
    fn a_stall_lowers_the_window_it_is_in_and_no_other() {
        let mut meter = RateMeter::default();
        let mut last = None;
        feed(&mut meter, &mut last, &[AT_30_FPS; 40]);
        feed(&mut meter, &mut last, &[AT_30_FPS; 2]);
        feed(&mut meter, &mut last, &[1000 * MS]);
        assert!(meter.measured().unwrap() < 15.0, "{:?}", meter.measured());
        feed(&mut meter, &mut last, &[AT_30_FPS; 35]);
        assert_eq!(meter.measured(), Some(30.0));
    }

    #[test]
    fn the_measurement_follows_the_camera() {
        let mut meter = RateMeter::default();
        let mut last = None;
        feed(&mut meter, &mut last, &[AT_30_FPS; 40]);
        assert_eq!(meter.measured(), Some(30.0));
        // The window under way when the rate changes, then one at the new rate.
        feed(&mut meter, &mut last, &[66_666_667; 32]);
        assert_eq!(meter.measured(), Some(15.0));
    }

    #[test]
    fn a_slow_camera_takes_no_longer_than_a_fast_one() {
        assert_eq!(measured_after(&[200 * MS; 8]), Some(5.0));
    }

    #[test]
    fn a_restart_forgets_the_stream() {
        let mut meter = RateMeter::default();
        let mut last = None;
        feed(&mut meter, &mut last, &[AT_30_FPS; 40]);
        assert!(meter.measured().is_some());
        meter.restart();
        assert_eq!(meter.measured(), None);
        meter.frame(0);
        assert_eq!(meter.measured(), None);
    }
}
