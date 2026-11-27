//! Guest clock alignment against the host, and the drift gate that
//! decides when a local seek is worth it.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::protocol::PlaybackAnchor;

/// Wall clock in milliseconds. Both sides stamp pings with this so the
/// offset is comparable across machines. `Instant` is not.
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One NTP sample: guest send, host receive, host send, guest receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    offset_ms: i64,
    rtt_ms: u64,
}

/// Sliding window of the lowest-RTT samples. The offset is their median,
/// so one slow relay hop does not drag playback.
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    samples: Vec<Sample>,
}

impl ClockSync {
    pub const WINDOW: usize = 8;

    pub fn observe(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) -> bool {
        let rtt = (t4 as i64).saturating_sub(t1 as i64) - (t3 as i64).saturating_sub(t2 as i64);
        // Negative RTT is a clock step mid-sample. Anything over 10s is
        // not a path we want steering playback.
        if !(0..=10_000).contains(&rtt) {
            return false;
        }
        let offset = ((t2 as i64) - (t1 as i64) + ((t3 as i64) - (t4 as i64))) / 2;
        self.samples.push(Sample {
            offset_ms: offset,
            rtt_ms: rtt as u64,
        });
        if self.samples.len() > Self::WINDOW {
            if let Some((worst, _)) = self
                .samples
                .iter()
                .enumerate()
                .max_by_key(|(_, s)| s.rtt_ms)
            {
                self.samples.remove(worst);
            }
        }
        true
    }

    /// Median offset of the samples we kept. Empty window → 0 (treat the
    /// guest clock as the host clock until the first pong lands).
    pub fn offset_ms(&self) -> i64 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut offs: Vec<i64> = self.samples.iter().map(|s| s.offset_ms).collect();
        offs.sort_unstable();
        offs[offs.len() / 2]
    }

    pub fn best_rtt_ms(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.rtt_ms).min()
    }

    pub fn est_host_now(&self, guest_now_ms: u64) -> u64 {
        guest_now_ms.saturating_add_signed(self.offset_ms())
    }
}

/// Where the host playhead should be, given the last anchor and our
/// estimate of the host's wall clock.
pub fn expected_position_ms(anchor: &PlaybackAnchor, est_host_now_ms: u64) -> u64 {
    if !anchor.playing {
        return anchor.position_ms;
    }
    let elapsed = est_host_now_ms.saturating_sub(anchor.anchor_host_ms);
    anchor.position_ms.saturating_add(elapsed)
}

/// Seek only when we are meaningfully off, and not again until the
/// previous seek has had time to land.
#[derive(Clone, Debug, Default)]
pub struct DriftGate {
    last_seek: Option<Instant>,
}

impl DriftGate {
    pub const THRESHOLD_MS: u64 = 350;
    pub const COOLDOWN: Duration = Duration::from_millis(2500);

    pub fn should_seek(&mut self, drift_ms: i64) -> bool {
        self.should_seek_at(drift_ms, Instant::now())
    }

    pub fn should_seek_at(&mut self, drift_ms: i64, now: Instant) -> bool {
        if drift_ms.unsigned_abs() < Self::THRESHOLD_MS {
            return false;
        }
        if self
            .last_seek
            .is_some_and(|t| now.saturating_duration_since(t) < Self::COOLDOWN)
        {
            return false;
        }
        self.last_seek = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_uses_the_median_of_the_lowest_rtt_window() {
        let mut clock = ClockSync::default();
        // Tight samples say the host is 1000ms ahead. One slow sample
        // says 9000ms and must fall out of the window once it is the
        // worst of nine.
        for _ in 0..8 {
            assert!(clock.observe(1_000, 2_100, 2_120, 1_200));
        }
        assert!(clock.observe(5_000, 14_500, 14_600, 5_800));
        assert_eq!(clock.samples.len(), 8);
        assert_eq!(clock.offset_ms(), 1_010);
        // (t4-t1) - (t3-t2) = 200 - 20 for the kept samples.
        assert_eq!(clock.best_rtt_ms(), Some(180));
        assert!(clock.est_host_now(10_000) > 10_000);
    }

    #[test]
    fn nonsense_rtt_is_dropped() {
        let mut clock = ClockSync::default();
        // t4 before t1, and the server delay does not explain it.
        assert!(!clock.observe(5_000, 5_010, 5_020, 1_000));
        assert_eq!(clock.offset_ms(), 0);
        assert!(clock.best_rtt_ms().is_none());
    }

    #[test]
    fn expected_position_holds_while_paused() {
        let anchor = PlaybackAnchor {
            playing: false,
            position_ms: 12_000,
            anchor_host_ms: 1_000,
            track_uid: Some(3),
            loop_one: false,
            shuffle: false,
        };
        assert_eq!(expected_position_ms(&anchor, 50_000), 12_000);
        let playing = PlaybackAnchor {
            playing: true,
            ..anchor
        };
        assert_eq!(expected_position_ms(&playing, 6_000), 17_000);
    }

    #[test]
    fn drift_gate_ignores_small_error_and_cools_down() {
        let mut gate = DriftGate::default();
        let t0 = Instant::now();
        assert!(!gate.should_seek_at(200, t0));
        assert!(gate.should_seek_at(800, t0));
        assert!(
            !gate.should_seek_at(900, t0 + Duration::from_millis(400)),
            "a second seek inside the cooldown would thrash the decoder"
        );
        assert!(gate.should_seek_at(-900, t0 + DriftGate::COOLDOWN));
    }
}
