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

/// How long an input waits for its window to change before it is given up:
/// a key that changes nothing on screen (a modifier, a no-op) has no answer.
const INPUT_TIMEOUT_MICROS: u64 = 2_000_000;

/// The stages of one input's answer, in order.
pub const INPUT_STAGES: [&str; 2] = ["input to capture", "input to screen"];

/// Input-to-screen latency: from this client sending a key or click to a
/// window, to the first frame of that window the Mac captured after the input
/// could have reached it, on screen here.
///
/// The host says nothing about which frame answers which input, so the first
/// change after the input is taken as its answer. In a window that changes only
/// when typed into, that is the keystroke's own redraw; a caret blink or
/// animation landing first can make a sample read early.
#[derive(Debug, Default)]
pub struct InputProbe {
    /// Window to the oldest unanswered input, on our clock.
    pending: std::collections::HashMap<u64, u64>,
    samples: VecDeque<[Option<f32>; 2]>,
}

impl InputProbe {
    /// An input went to `window` at `at` (ours). An older unanswered one stands:
    /// the next change answers it first.
    pub fn on_input(&mut self, window: u64, at: u64) {
        let entry = self.pending.entry(window).or_insert(at);
        if at.saturating_sub(*entry) > INPUT_TIMEOUT_MICROS {
            *entry = at;
        }
    }

    /// A frame of `window` reached its end (presented, or received where nothing
    /// presents). Records a sample when it answers an input; true if it did.
    pub fn on_frame(&mut self, window: u64, stamps: &FrameStamps, clock: &ClockSync) -> bool {
        let Some(&sent) = self.pending.get(&window) else {
            return false;
        };
        let end = if stamps.presented != 0 { stamps.presented } else { stamps.received };
        if end.saturating_sub(sent) > INPUT_TIMEOUT_MICROS {
            self.pending.remove(&window);
            return false;
        }
        let (Some(captured), Some(rtt)) = (clock.to_client(stamps.captured), clock.min_rtt_micros())
        else {
            return false;
        };
        // Captured before the input could have arrived: not its answer.
        if stamps.captured == 0 || captured < sent + rtt / 2 {
            return false;
        }
        self.pending.remove(&window);
        let ms = |from: u64, to: u64| (to >= from).then(|| (to - from) as f32 / 1000.0);
        self.samples.push_back([ms(sent, captured), ms(sent, end)]);
        if self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
        true
    }

    /// The `q` quantile (0..=1) of stage `stage` over the recent answers.
    pub fn percentile(&self, stage: usize, q: f32) -> Option<f32> {
        let mut values: Vec<f32> = self.samples.iter().filter_map(|s| s[stage]).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(f32::total_cmp);
        Some(values[((values.len() - 1) as f32 * q).round() as usize])
    }

    /// `input to capture 30.1/41.0  input to screen 41.2/55.3 ms (p50/p95, 20
    /// inputs)`, or None before the first answer.
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = INPUT_STAGES
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let (p50, p95) = (self.percentile(i, 0.5)?, self.percentile(i, 0.95)?);
                Some(format!("{name} {p50:.1}/{p95:.1}"))
            })
            .collect();
        (!parts.is_empty()).then(|| {
            format!("{} ms (p50/p95, {} inputs)", parts.join("  "), self.samples.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_is_answered_by_the_first_frame_captured_after_it_arrived() {
        let mut clock = ClockSync::default();
        clock.on_pong(0, 1_000_000, 4_000); // rtt 4 ms; host = ours + 998_000
        let mut probe = InputProbe::default();
        probe.on_input(7, 10_000);
        probe.on_input(7, 12_000); // the older one stands
        let frame = |captured_host: u64, received: u64| FrameStamps {
            captured: captured_host,
            received,
            ..Default::default()
        };
        // Captured 1 ms after the key was sent: before it could arrive.
        assert!(!probe.on_frame(7, &frame(1_009_000, 14_000), &clock));
        // Another window's frame is not an answer.
        assert!(!probe.on_frame(8, &frame(1_030_000, 35_000), &clock));
        assert!(probe.on_frame(7, &frame(1_030_000, 36_000), &clock));
        assert_eq!(probe.percentile(0, 0.5), Some(22.0));
        assert_eq!(probe.percentile(1, 0.5), Some(26.0));
        // Answered: the next frame is not counted again.
        assert!(!probe.on_frame(7, &frame(1_040_000, 46_000), &clock));
        let text = probe.summary().unwrap();
        assert!(text.starts_with("input to capture 22.0/22.0  input to screen 26.0/26.0 ms"), "{text}");
    }

    #[test]
    fn an_input_nothing_answers_is_given_up() {
        let mut clock = ClockSync::default();
        clock.on_pong(0, 0, 0);
        let mut probe = InputProbe::default();
        probe.on_input(1, 1_000);
        let late = FrameStamps {
            captured: 3_500_000,
            received: 3_600_000,
            ..Default::default()
        };
        assert!(!probe.on_frame(1, &late, &clock));
        assert_eq!(probe.summary(), None);
    }

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
