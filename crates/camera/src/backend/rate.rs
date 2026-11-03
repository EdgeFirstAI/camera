// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Drop counting against the reported frame rate, and the delivered rate
//! measured from capture timestamps.
//!
//! `config()` reports the rate the driver or pipeline claims. Frames are
//! counted lost against that claim, so a camera slower than its claim
//! shows drops and a faster one none. The measured rate is reported
//! separately, by `Camera::measured_frame_rate`, and a measurement that
//! contradicts the claim is logged. Frames are never dropped to meet a
//! rate.

/// Frames whose timestamp intervals give the measured rate.
pub(crate) const RATE_FRAMES: usize = 10;

/// Relative difference between the claimed and measured rates that is
/// logged.
const TOLERANCE: f64 = 0.01;

/// Counts drops against the claimed rate and measures the delivered one.
#[derive(Debug, Default)]
pub(crate) struct RateMeter {
    claimed: Option<f64>,
    last_nanos: Option<i64>,
    intervals: Vec<i64>,
    measured: Option<f64>,
}

/// What one frame told the meter.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Tick {
    /// Frames lost since the previous one, against the claimed rate.
    pub(crate) dropped: u64,
    /// The claimed and measured rates, on the frame that completed a
    /// measurement contradicting the claim.
    pub(crate) disagrees: Option<(f64, f64)>,
}

impl RateMeter {
    /// Sets the rate the driver or pipeline claims and measures again.
    pub(crate) fn claim(&mut self, fps: Option<f64>) {
        self.claimed = fps.filter(|f| f.is_finite() && *f > 0.0);
        self.intervals.clear();
        self.measured = None;
    }

    /// Forgets the stream, for a stop: the next frame starts no interval.
    pub(crate) fn restart(&mut self) {
        self.last_nanos = None;
        self.intervals.clear();
        self.measured = None;
    }

    /// The delivered rate, once measured since the last start or claim.
    pub(crate) fn measured(&self) -> Option<f64> {
        self.measured
    }

    /// Takes the capture timestamp of a delivered frame.
    pub(crate) fn frame(&mut self, nanos: i64) -> Tick {
        let mut tick = Tick::default();
        let Some(last) = self.last_nanos.replace(nanos) else {
            return tick;
        };
        let interval = nanos - last;
        if interval <= 0 {
            return tick;
        }
        if let Some(fps) = self.claimed {
            let periods = (interval as f64 * fps / 1e9).round() as i64;
            tick.dropped = periods.saturating_sub(1).max(0) as u64;
        }
        if self.measured.is_none() {
            self.intervals.push(interval);
            if self.intervals.len() == RATE_FRAMES {
                // The median ignores intervals that span a lost frame.
                self.intervals.sort_unstable();
                let fps = round_fps(1e9 / self.intervals[RATE_FRAMES / 2] as f64);
                self.measured = Some(fps);
                tick.disagrees = self
                    .claimed
                    .filter(|c| (fps - c).abs() > c * TOLERANCE)
                    .map(|c| (c, fps));
            }
        }
        tick
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

    fn feed(meter: &mut RateMeter, period: i64, n: usize, start: i64) -> (Vec<Tick>, i64) {
        let mut t = start;
        let ticks = (0..n)
            .map(|_| {
                let tick = meter.frame(t);
                t += period;
                tick
            })
            .collect();
        (ticks, t)
    }

    #[test]
    fn a_camera_faster_than_its_claim_drops_nothing_and_is_measured() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let (ticks, _) = feed(&mut m, 16_667 * 1000, RATE_FRAMES + 1, 0);
        assert!(ticks.iter().all(|t| t.dropped == 0));
        assert_eq!(ticks[RATE_FRAMES].disagrees, Some((30.0, 60.0)));
        assert_eq!(m.measured(), Some(60.0));
    }

    #[test]
    fn a_camera_slower_than_its_claim_counts_drops() {
        let mut m = RateMeter::default();
        m.claim(Some(60.0));
        let (ticks, _) = feed(&mut m, 33_333_333, RATE_FRAMES + 1, 0);
        assert_eq!(
            ticks.iter().map(|t| t.dropped).sum::<u64>(),
            RATE_FRAMES as u64
        );
        assert_eq!(m.measured(), Some(30.0));
    }

    #[test]
    fn a_matching_claim_does_not_disagree() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let (ticks, _) = feed(&mut m, 33_333_333, RATE_FRAMES + 1, 0);
        assert!(ticks.iter().all(|t| *t == Tick::default()));
        assert_eq!(m.measured(), Some(30.0));
    }

    #[test]
    fn a_lost_frame_counts_once_and_does_not_skew_the_measurement() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut t = 0;
        let mut dropped = 0;
        for i in 0..=RATE_FRAMES {
            dropped += m.frame(t).dropped;
            t += if i == 3 { 66 * MS } else { 33 * MS };
        }
        assert_eq!(dropped, 1);
        assert_eq!(m.measured(), Some(round_fps(1e9 / (33 * MS) as f64)));
    }

    #[test]
    fn no_claim_counts_no_drops_but_still_measures() {
        let mut m = RateMeter::default();
        let (ticks, _) = feed(&mut m, 100 * MS, RATE_FRAMES + 1, 0);
        assert!(ticks.iter().all(|t| *t == Tick::default()));
        assert_eq!(m.measured(), Some(10.0));
    }

    #[test]
    fn a_new_claim_measures_again_and_a_restart_starts_no_interval() {
        let mut m = RateMeter::default();
        let (_, t) = feed(&mut m, 33 * MS, RATE_FRAMES + 1, 0);
        assert!(m.measured().is_some());
        m.claim(Some(15.0));
        assert_eq!(m.measured(), None);
        m.restart();
        assert_eq!(m.frame(t + 1000 * MS), Tick::default());
    }
}
