// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Drop counting against the frame duration, and the delivered rate measured
//! from capture timestamps.
//!
//! A frame is counted lost when the interval before it spans more than one
//! frame duration. Where the backend reports each frame's duration that is
//! the duration used, so auto-exposure lengthening its frames shows no drops.
//! Elsewhere it is the rate the driver claims, so a camera slower than its
//! claim shows drops and a faster one none.
//!
//! `config()` reports the claimed rate. The measured rate is reported
//! separately, by `Camera::measured_frame_rate`, and a measurement that
//! contradicts the claim or the request is logged once. Frames are never
//! dropped to meet a rate.

use std::fmt;

use crate::rates_differ;

/// Frames whose timestamp intervals give the measured rate.
pub(crate) const RATE_FRAMES: usize = 10;

/// Counts drops against the frame duration and measures the delivered rate.
#[derive(Debug, Default)]
pub(crate) struct RateMeter {
    claimed: Option<f64>,
    asked: Option<f64>,
    last_nanos: Option<i64>,
    last_duration: Option<i64>,
    intervals: Vec<i64>,
    measured: Option<f64>,
}

/// What one frame told the meter.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Tick {
    /// Frames lost since the previous one.
    pub(crate) dropped: u64,
    /// On the frame that completed a measurement contradicting the claim or
    /// the request.
    pub(crate) mismatch: Option<RateMismatch>,
}

/// A measured rate that contradicts what the camera claims or was asked for.
#[derive(Debug, PartialEq)]
pub(crate) struct RateMismatch {
    measured: f64,
    claimed: Option<f64>,
    asked: Option<f64>,
}

impl fmt::Display for RateMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the camera delivers {} fps", self.measured)?;
        match self.claimed {
            Some(claimed) => write!(f, "; it reports {claimed} fps")?,
            None => write!(f, "; it reports no frame rate")?,
        }
        if let Some(asked) = self.asked {
            write!(f, ", and {asked} fps was requested")?;
        }
        Ok(())
    }
}

impl RateMeter {
    /// Sets the rate the driver or pipeline claims and measures again.
    pub(crate) fn claim(&mut self, fps: Option<f64>) {
        self.claimed = fps.filter(|f| f.is_finite() && *f > 0.0);
        self.intervals.clear();
        self.measured = None;
    }

    /// Sets the rate that was requested, which a measurement is checked
    /// against as well as the claim. Only libcamera needs this: a V4L2 driver
    /// answers `S_PARM` with the rate it will use, while a libcamera pipeline
    /// can ignore a request without saying so.
    #[cfg(any(test, feature = "libcamera"))]
    pub(crate) fn ask(&mut self, fps: f64) {
        self.asked = Some(fps);
    }

    /// Forgets the stream, for a stop: the next frame starts no interval.
    pub(crate) fn restart(&mut self) {
        self.last_nanos = None;
        self.last_duration = None;
        self.intervals.clear();
        self.measured = None;
    }

    /// The delivered rate, once measured since the last start or claim.
    pub(crate) fn measured(&self) -> Option<f64> {
        self.measured
    }

    /// Takes the capture timestamp of a delivered frame and, where the backend
    /// reports it, the frame's own duration.
    pub(crate) fn frame(&mut self, nanos: i64, duration_nanos: Option<i64>) -> Tick {
        let mut tick = Tick::default();
        let duration = duration_nanos.filter(|d| *d > 0);
        let previous_duration = std::mem::replace(&mut self.last_duration, duration);
        let Some(last) = self.last_nanos.replace(nanos) else {
            return tick;
        };
        let interval = nanos - last;
        if interval <= 0 {
            return tick;
        }
        // Timestamps mark the start of a frame, so the interval is the previous
        // frame's duration. Taking the longer of the two also absorbs a frame
        // whose reported duration is a step behind an exposure change.
        let period = duration
            .max(previous_duration)
            .map(|d| d as f64)
            .or_else(|| self.claimed.map(|fps| 1e9 / fps));
        if let Some(period) = period {
            let periods = (interval as f64 / period).round() as i64;
            tick.dropped = periods.saturating_sub(1).max(0) as u64;
        }
        if self.measured.is_none() {
            self.intervals.push(interval);
            if self.intervals.len() == RATE_FRAMES {
                // The median ignores intervals that span a lost frame.
                self.intervals.sort_unstable();
                let fps = round_fps(1e9 / self.intervals[RATE_FRAMES / 2] as f64);
                self.measured = Some(fps);
                let contradicted = self.claimed.is_none_or(|c| rates_differ(fps, c))
                    || self.asked.is_some_and(|a| rates_differ(fps, a));
                tick.mismatch = contradicted.then_some(RateMismatch {
                    measured: fps,
                    claimed: self.claimed,
                    asked: self.asked,
                });
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
                let tick = meter.frame(t, None);
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
        assert_eq!(
            ticks[RATE_FRAMES].mismatch.as_ref().map(|m| m.to_string()),
            Some("the camera delivers 60 fps; it reports 30 fps".to_owned())
        );
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
            dropped += m.frame(t, None).dropped;
            t += if i == 3 { 66 * MS } else { 33 * MS };
        }
        assert_eq!(dropped, 1);
        assert_eq!(m.measured(), Some(round_fps(1e9 / (33 * MS) as f64)));
    }

    #[test]
    fn no_claim_counts_no_drops_but_still_measures() {
        let mut m = RateMeter::default();
        let (ticks, _) = feed(&mut m, 100 * MS, RATE_FRAMES + 1, 0);
        assert!(ticks.iter().all(|t| t.dropped == 0));
        assert_eq!(
            ticks[RATE_FRAMES].mismatch.as_ref().map(|m| m.to_string()),
            Some("the camera delivers 10 fps; it reports no frame rate".to_owned())
        );
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
        assert_eq!(m.frame(t + 1000 * MS, None), Tick::default());
    }

    #[test]
    fn frames_lengthened_by_exposure_are_not_drops() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut t = 0;
        let mut dropped = 0;
        // The interval before each frame and its reported duration, which
        // lags the sensor by a frame while the exposure ramps.
        for (interval, duration) in [(0, 33), (33, 33), (45, 33), (67, 45), (67, 67), (67, 67)] {
            t += interval * MS;
            dropped += m.frame(t, Some(duration * MS)).dropped;
        }
        assert_eq!(dropped, 0);
    }

    #[test]
    fn a_frame_lost_at_a_long_exposure_counts_once() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        m.frame(0, Some(67 * MS));
        assert_eq!(m.frame(67 * MS, Some(67 * MS)).dropped, 0);
        assert_eq!(m.frame(201 * MS, Some(67 * MS)).dropped, 1);
    }

    #[test]
    fn an_unusable_duration_falls_back_to_the_claim() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        m.frame(0, Some(-1));
        assert_eq!(m.frame(66 * MS, Some(-1)).dropped, 1);
    }

    #[test]
    fn one_measurement_reports_the_claim_and_the_request_together() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        m.ask(60.0);
        let (ticks, _) = feed(&mut m, 33_333_333, RATE_FRAMES + 1, 0);
        let reports: Vec<_> = ticks
            .iter()
            .filter_map(|t| t.mismatch.as_ref().map(|m| m.to_string()))
            .collect();
        assert_eq!(
            reports,
            ["the camera delivers 30 fps; it reports 30 fps, and 60 fps was requested"]
        );
    }
}
