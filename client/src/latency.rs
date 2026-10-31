//! Video latency, stage by stage (M3): where the time goes between the Mac
//! compositing a frame and this client presenting it.
//!
//! Every frame carries host-clock stamps for its host stages (protocol.md §6,
//! v3); the client stamps the rest on its own clock. `ClockSync` maps one clock
//! onto the other from `ping`/`pong` round trips, taking the fastest recent
//! round trip as the most accurate, so only the network stage depends on it.
//!
//! Pure and platform-free, so it is unit tested anywhere.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

/// The client clock: microseconds since this process first asked.
pub fn now_micros() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Round trips kept for the offset estimate.
const CLOCK_SAMPLES: usize = 32;

/// The offset between the host clock and ours, from ping/pong round trips.
#[derive(Debug, Default)]
pub struct ClockSync {
    /// (round trip, host minus client at the midpoint), newest last.
    samples: VecDeque<(u64, i64)>,
}

impl ClockSync {
    /// A pong for a ping sent at `sent` (ours) arrived at `received` (ours);
    /// the host read it at `host` (host clock).
    pub fn on_pong(&mut self, sent: u64, host: u64, received: u64) {
        let Some(rtt) = received.checked_sub(sent) else {
            return;
        };
        let midpoint = sent + rtt / 2;
        self.samples.push_back((rtt, host as i64 - midpoint as i64));
        if self.samples.len() > CLOCK_SAMPLES {
            self.samples.pop_front();
        }
    }

    fn best(&self) -> Option<(u64, i64)> {
        self.samples.iter().copied().min_by_key(|(rtt, _)| *rtt)
    }

    /// The fastest recent round trip, in microseconds.
    pub fn min_rtt_micros(&self) -> Option<u64> {
        self.best().map(|(rtt, _)| rtt)
    }

    /// `host` (host clock) on our clock, once there is a round trip to go by.
    pub fn to_client(&self, host: u64) -> Option<u64> {
        let (_, offset) = self.best()?;
        u64::try_from(host as i64 - offset).ok()
    }
}

/// One frame's journey. Host stamps are on the host clock, the rest on ours;
/// zero means not reached (or not measured by this consumer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStamps {
    pub composited: u64,
    pub captured: u64,
    pub encoded: u64,
    pub sent: u64,
    pub received: u64,
    pub submitted: u64,
    pub decoded: u64,
    pub presented: u64,
}

/// The stages a report breaks a frame into, in order, and the whole.
pub const STAGES: [&str; 8] = [
    "capture", "encode", "send", "network", "queue", "decode", "display", "total",
];

impl FrameStamps {
    /// Each stage's milliseconds, `None` where a stamp is missing.
    pub fn stages(&self, clock: &ClockSync) -> [Option<f32>; 8] {
        fn span(from: u64, to: u64) -> Option<f32> {
            (from != 0 && to != 0 && to >= from).then(|| (to - from) as f32 / 1000.0)
        }
        let sent = clock.to_client(self.sent).unwrap_or(0);
        // ScreenCaptureKit stamps a frame with when it is due on screen, which
        // can fall after it reaches us: the frame starts at the earlier stamp.
        let start = match (self.composited, self.captured) {
            (0, c) | (c, 0) => c,
            (a, c) => a.min(c),
        };
        let start = clock.to_client(start).unwrap_or(0);
        let end = if self.presented != 0 { self.presented } else { self.received };
        [
            span(self.composited, self.captured),
            span(self.captured, self.encoded),
            span(self.encoded, self.sent),
            span(sent, self.received),
            span(self.received, self.submitted),
            span(self.submitted, self.decoded),
            span(self.decoded, self.presented),
            span(start, end),
        ]
    }
}

/// Frames kept for the percentiles: a few seconds at 60fps.
const WINDOW: usize = 300;

/// Rolling per-stage latency of one lane.
#[derive(Debug, Default)]
pub struct LatencyStats {
    frames: VecDeque<[Option<f32>; 8]>,
}

impl LatencyStats {
    pub fn record(&mut self, stamps: &FrameStamps, clock: &ClockSync) {
        self.frames.push_back(stamps.stages(clock));
        if self.frames.len() > WINDOW {
            self.frames.pop_front();
        }
    }

    /// The `q` quantile (0..=1) of stage `stage` over the window.
    pub fn percentile(&self, stage: usize, q: f32) -> Option<f32> {
        let mut values: Vec<f32> = self.frames.iter().filter_map(|f| f[stage]).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(f32::total_cmp);
        let i = ((values.len() - 1) as f32 * q).round() as usize;
        Some(values[i])
    }

    /// `capture 0.4/0.9  encode 3.1/4.0 ... total 28.0/41.2 ms` (p50/p95), the
    /// stages this consumer measures only.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (i, name) in STAGES.iter().enumerate() {
            if let (Some(p50), Some(p95)) = (self.percentile(i, 0.5), self.percentile(i, 0.95)) {
                parts.push(format!("{name} {p50:.1}/{p95:.1}"));
            }
        }
        format!("{} ms (p50/p95, {} frames)", parts.join("  "), self.frames.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sync_uses_the_fastest_round_trip() {
        let mut clock = ClockSync::default();
        assert_eq!(clock.to_client(5_000), None);
        // Host runs 1_000_000 ahead. A slow trip with all delay on the way back
        // reads the host early; the fast one should win.
        clock.on_pong(100, 1_000_150, 10_100);
        clock.on_pong(20_000, 1_020_500, 21_000);
        assert_eq!(clock.min_rtt_micros(), Some(1_000));
        assert_eq!(clock.to_client(1_020_500), Some(20_500));
        // A pong from before the ping (clock nonsense) is ignored.
        clock.on_pong(50_000, 1, 40_000);
        assert_eq!(clock.min_rtt_micros(), Some(1_000));
    }

    #[test]
    fn stages_split_a_frame_and_skip_missing_stamps() {
        let mut clock = ClockSync::default();
        clock.on_pong(0, 999_000, 0); // host = ours + 999_000
        let stamps = FrameStamps {
            composited: 1_000_000,
            captured: 1_001_000,
            encoded: 1_005_000,
            sent: 1_005_500,
            received: 8_500,
            submitted: 9_000,
            decoded: 15_000,
            presented: 0,
        };
        let s = stamps.stages(&clock);
        assert_eq!(s[0], Some(1.0));
        assert_eq!(s[1], Some(4.0));
        assert_eq!(s[2], Some(0.5));
        assert_eq!(s[3], Some(2.0));
        assert_eq!(s[4], Some(0.5));
        assert_eq!(s[5], Some(6.0));
        assert_eq!(s[6], None);
        // Not presented yet: the whole runs to receipt.
        assert_eq!(s[7], Some(7.5));
        // Stamped as due on screen after the capture had it: the capture stage is
        // unknown and the whole starts at the capture.
        let late = FrameStamps {
            composited: 1_002_000,
            ..stamps
        };
        let s = late.stages(&clock);
        assert_eq!(s[0], None);
        assert_eq!(s[7], Some(6.5));
    }

    #[test]
    fn summary_reports_percentiles_of_measured_stages() {
        let mut clock = ClockSync::default();
        clock.on_pong(0, 0, 0);
        let mut stats = LatencyStats::default();
        for i in 1..=100u64 {
            stats.record(
                &FrameStamps {
                    composited: 1_000,
                    captured: 1_000 + i * 10,
                    ..Default::default()
                },
                &clock,
            );
        }
        assert_eq!(stats.percentile(0, 0.5), Some(0.51));
        assert_eq!(stats.percentile(0, 0.95), Some(0.95));
        assert_eq!(stats.percentile(1, 0.5), None);
        let text = stats.summary();
        assert!(text.starts_with("capture 0.5/0.9 ms"), "{text}");
        assert!(!text.contains("encode"), "{text}");
    }
}
