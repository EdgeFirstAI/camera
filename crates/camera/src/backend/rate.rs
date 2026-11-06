// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Au-Zone Technologies. All Rights Reserved.

//! Drop counting and the delivered rate, both from capture timestamps.
//!
//! A frame's period is taken from the frames around it. A lost frame is a long
//! interval among short ones, while a change of rate persists, so auto-exposure
//! or a new frame rate counts no drops however abruptly it takes effect. The
//! price is that a verdict comes two frames later, and that a steady loss, such
//! as every other frame, looks like a slower camera: it shows in the measured
//! rate instead.
//!
//! Nothing here relies on what the camera says about its frames, because that
//! does not hold across cameras: the sequence number shows no gaps on PiSP, neo
//! or vvcam, neo's frame duration never changes, and not every V4L2 driver
//! reports a rate.
//!
//! `config()` reports the rate the camera claims. The measured rate is reported
//! separately, by `Camera::measured_frame_rate`, and follows the camera. Once a
//! measurement that contradicts the claim or the request has settled, it is
//! logged, once until the rate changes again. Frames are never dropped to meet
//! a rate.

use std::fmt;

use crate::rates_differ;

/// Frames whose timestamp intervals give each measurement of the rate.
pub(crate) const RATE_FRAMES: usize = 10;

/// Intervals around the one being judged, which is in the middle.
const DROP_WINDOW: usize = 5;

/// Counts drops and measures the delivered rate.
#[derive(Debug, Default)]
pub(crate) struct RateMeter {
    claimed: Option<f64>,
    asked: Option<f64>,
    last_nanos: Option<i64>,
    /// The latest intervals, oldest first, once `filled` reaches the window.
    window: [i64; DROP_WINDOW],
    filled: usize,
    /// Intervals of the measurement in progress.
    intervals: Vec<i64>,
    measured: Option<f64>,
    /// The rate last logged as contradicting the claim or the request.
    warned: Option<f64>,
}

/// What one frame told the meter.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Tick {
    /// Frames found lost, up to two frames before this one.
    pub(crate) dropped: u64,
    /// On the frame that settled a measurement contradicting the claim or the
    /// request.
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
        self.warned = None;
    }

    /// Updates the claim from a running camera, which measuring carries on
    /// through.
    #[cfg(any(test, feature = "libcamera"))]
    pub(crate) fn reports(&mut self, fps: f64) {
        self.claimed = Some(fps);
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
        self.filled = 0;
        self.intervals.clear();
        self.measured = None;
        self.warned = None;
    }

    /// The delivered rate over the latest measurement since the last start or
    /// claim.
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
        tick.dropped = self.judge(interval);
        self.intervals.push(interval);
        if self.intervals.len() == RATE_FRAMES {
            tick.mismatch = self.measure();
        }
        tick
    }

    /// Takes the latest interval and returns the frames lost in the one now in
    /// the middle of the window. The first two of a start, which include the
    /// sensor starting up, are never in the middle.
    fn judge(&mut self, interval: i64) -> u64 {
        if self.filled < DROP_WINDOW {
            self.window[self.filled] = interval;
            self.filled += 1;
        } else {
            self.window.copy_within(1.., 0);
            self.window[DROP_WINDOW - 1] = interval;
        }
        if self.filled < DROP_WINDOW {
            return 0;
        }
        let middle = DROP_WINDOW / 2;
        let mut sorted = self.window;
        sorted.sort_unstable();
        // The median alone would take losses on alternate frames for the
        // period. Next to a change of rate, one neighbour is at the new rate.
        let period = sorted[middle].min(self.window[middle - 1].max(self.window[middle + 1]));
        let periods = self.window[middle] as f64 / period as f64;
        (periods.round() as u64).saturating_sub(1)
    }

    /// Ends a measurement. It is only logged once the next one agrees, so that
    /// the frames during a change of rate, which take it to neither rate, are
    /// not reported as the camera's rate.
    fn measure(&mut self) -> Option<RateMismatch> {
        // The lower median ignores intervals that span a lost frame, even
        // when half of them do.
        self.intervals.sort_unstable();
        let fps = round_fps(1e9 / self.intervals[(RATE_FRAMES - 1) / 2] as f64);
        self.intervals.clear();
        let settled = self.measured.is_some_and(|last| !rates_differ(fps, last));
        self.measured = Some(fps);
        let contradicted = self.claimed.is_none_or(|c| rates_differ(fps, c))
            || self.asked.is_some_and(|a| rates_differ(fps, a));
        if !contradicted {
            self.warned = None;
            return None;
        }
        if !settled || self.warned.is_some_and(|w| !rates_differ(fps, w)) {
            return None;
        }
        self.warned = Some(fps);
        Some(RateMismatch {
            measured: fps,
            claimed: self.claimed,
            asked: self.asked,
        })
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
    /// A window of steady frames either side of what a test is about.
    const STEADY: [i64; 6] = [33 * MS; 6];

    /// Feeds frames at `intervals` and returns their ticks.
    fn feed(meter: &mut RateMeter, intervals: &[i64]) -> Vec<Tick> {
        let mut t = meter.last_nanos.unwrap_or(0);
        let mut ticks = Vec::with_capacity(intervals.len() + 1);
        if meter.last_nanos.is_none() {
            ticks.push(meter.frame(t));
        }
        for interval in intervals {
            t += interval;
            ticks.push(meter.frame(t));
        }
        ticks
    }

    fn dropped(ticks: &[Tick]) -> u64 {
        ticks.iter().map(|t| t.dropped).sum()
    }

    fn reports(ticks: &[Tick]) -> Vec<String> {
        ticks
            .iter()
            .filter_map(|t| t.mismatch.as_ref().map(ToString::to_string))
            .collect()
    }

    /// `ms`, between steady 33 ms frames.
    fn among_steady(ms: &[i64]) -> Vec<i64> {
        let mut all = STEADY.to_vec();
        all.extend(ms.iter().map(|m| m * MS));
        all.extend(STEADY);
        all
    }

    #[test]
    fn a_lost_frame_counts_once_and_does_not_skew_the_measurement() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let ticks = feed(&mut m, &among_steady(&[66]));
        assert_eq!(dropped(&ticks), 1);
        assert_eq!(m.measured(), Some(round_fps(1e9 / (33 * MS) as f64)));
    }

    #[test]
    fn neighbouring_losses_all_count() {
        let mut m = RateMeter::default();
        assert_eq!(dropped(&feed(&mut m, &among_steady(&[66, 66]))), 2);
    }

    #[test]
    fn losses_on_alternate_intervals_all_count() {
        let mut m = RateMeter::default();
        let ticks = feed(&mut m, &among_steady(&[66, 33, 66, 33, 66]));
        assert_eq!(dropped(&ticks), 3);

        let mut m = RateMeter::default();
        let losing: Vec<_> = [33 * MS, 66 * MS].repeat(RATE_FRAMES * 2);
        // All but one in the first two intervals and one with no verdict yet.
        assert_eq!(dropped(&feed(&mut m, &losing)), RATE_FRAMES as u64 * 2 - 2);
        assert_eq!(m.measured(), Some(round_fps(1e9 / (33 * MS) as f64)));
    }

    /// The OV5640 on the ISI: a stall whose count its driver's sequence
    /// number confirmed, on a driver that reports no rate.
    #[test]
    fn a_stall_counts_every_frame_it_spans_without_a_claim() {
        let mut m = RateMeter::default();
        let mut intervals = vec![33_333_333; 6];
        intervals.push(999_900_000);
        intervals.extend([33_333_333; 6]);
        assert_eq!(dropped(&feed(&mut m, &intervals)), 29);
    }

    #[test]
    fn an_abrupt_rate_change_is_not_a_drop_whatever_the_claim_says() {
        // The rate changes at once, with the claim behind it or ahead of it.
        for claim in [None, Some(15.0), Some(30.0)] {
            let mut m = RateMeter::default();
            m.claim(claim);
            let longer = [67 * MS; 6];
            let mut intervals = STEADY.to_vec();
            intervals.extend(longer);
            intervals.extend(STEADY);
            assert_eq!(dropped(&feed(&mut m, &intervals)), 0, "claim {claim:?}");
        }
    }

    /// What vvcam does when asked for 30 fps at 10 fps: two rates in between,
    /// after `S_PARM` has already answered.
    #[test]
    fn frames_on_the_way_to_a_new_rate_are_not_drops() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut intervals = vec![100 * MS; 6];
        intervals.extend([90 * MS; 4]);
        intervals.extend(STEADY);
        assert_eq!(dropped(&feed(&mut m, &intervals)), 0);
    }

    #[test]
    fn a_camera_steadily_slower_than_its_claim_drops_nothing_and_is_reported() {
        let mut m = RateMeter::default();
        m.claim(Some(60.0));
        let ticks = feed(&mut m, &[33_333_333; RATE_FRAMES * 3]);
        assert_eq!(dropped(&ticks), 0);
        assert_eq!(m.measured(), Some(30.0));
        assert_eq!(
            reports(&ticks),
            ["the camera delivers 30 fps; it reports 60 fps"]
        );
    }

    #[test]
    fn a_slow_start_is_not_a_drop() {
        let mut m = RateMeter::default();
        let mut intervals = vec![348 * MS];
        intervals.extend([33 * MS; 12]);
        assert_eq!(dropped(&feed(&mut m, &intervals)), 0);
    }

    #[test]
    fn the_measurement_follows_the_camera() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        feed(&mut m, &[33_333_333; RATE_FRAMES]);
        assert_eq!(m.measured(), Some(30.0));
        feed(&mut m, &[66_666_667; RATE_FRAMES]);
        assert_eq!(m.measured(), Some(15.0));
        feed(&mut m, &[100 * MS; RATE_FRAMES]);
        assert_eq!(m.measured(), Some(10.0));
    }

    #[test]
    fn a_matching_claim_does_not_disagree() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let ticks = feed(&mut m, &[33_333_333; RATE_FRAMES * 3]);
        assert!(ticks.iter().all(|t| *t == Tick::default()));
        assert_eq!(m.measured(), Some(30.0));
    }

    #[test]
    fn a_claim_that_follows_the_camera_does_not_disagree() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut ticks = feed(&mut m, &[33_333_333; RATE_FRAMES * 2]);
        m.reports(15.0);
        ticks.extend(feed(&mut m, &[66_666_667; RATE_FRAMES * 2]));
        assert_eq!(reports(&ticks), Vec::<String>::new());
        assert_eq!(m.measured(), Some(15.0));
    }

    #[test]
    fn no_claim_is_reported_and_still_measures() {
        let mut m = RateMeter::default();
        let ticks = feed(&mut m, &[100 * MS; RATE_FRAMES * 2]);
        assert_eq!(dropped(&ticks), 0);
        assert_eq!(
            reports(&ticks),
            ["the camera delivers 10 fps; it reports no frame rate"]
        );
        assert_eq!(m.measured(), Some(10.0));
    }

    #[test]
    fn a_contradiction_is_logged_once_per_rate_and_not_while_it_changes() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut intervals = vec![66_666_667; RATE_FRAMES * 3];
        // Each rate only for the frames of one measurement: not settled.
        intervals.extend([50 * MS; RATE_FRAMES]);
        intervals.extend([100 * MS; RATE_FRAMES * 3]);
        assert_eq!(
            reports(&feed(&mut m, &intervals)),
            [
                "the camera delivers 15 fps; it reports 30 fps",
                "the camera delivers 10 fps; it reports 30 fps",
            ]
        );
    }

    #[test]
    fn a_contradiction_that_returns_is_logged_again() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        let mut intervals = vec![66_666_667; RATE_FRAMES * 2];
        intervals.extend([33_333_333; RATE_FRAMES * 2]);
        intervals.extend([66_666_667; RATE_FRAMES * 2]);
        assert_eq!(reports(&feed(&mut m, &intervals)).len(), 2);
    }

    #[test]
    fn a_new_claim_measures_again_and_a_restart_starts_no_interval() {
        let mut m = RateMeter::default();
        feed(&mut m, &[33 * MS; RATE_FRAMES]);
        assert!(m.measured().is_some());
        m.claim(Some(15.0));
        assert_eq!(m.measured(), None);
        let t = m.last_nanos.unwrap();
        m.restart();
        assert_eq!(m.frame(t + 1000 * MS), Tick::default());
        assert_eq!(m.filled, 0);
    }

    #[test]
    fn one_measurement_reports_the_claim_and_the_request_together() {
        let mut m = RateMeter::default();
        m.claim(Some(30.0));
        m.ask(60.0);
        let ticks = feed(&mut m, &[33_333_333; RATE_FRAMES * 2]);
        assert_eq!(
            reports(&ticks),
            ["the camera delivers 30 fps; it reports 30 fps, and 60 fps was requested"]
        );
    }
}
