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

/// Recent NTP samples. The offset is the median of the lowest-RTT ones
/// in a FIFO window, so a relay spike does not drag playback and a later
/// clock step or a better path can still take over.
#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    samples: Vec<Sample>,
    /// Join-time offset from the welcome timestamp. Used only until the
    /// first real pong, so two machines with different wall clocks do not
    /// start several seconds apart.
    fallback: Option<i64>,
}

impl ClockSync {
    pub const WINDOW: usize = 12;
    /// How many of the lowest-RTT samples vote on the offset.
    const BEST: usize = 5;

    pub fn seed_fallback(&mut self, host_now_ms: u64, guest_now_ms: u64) {
        self.fallback = Some((host_now_ms as i64).saturating_sub(guest_now_ms as i64));
    }

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
            self.samples.remove(0);
        }
        true
    }

    /// Median offset of the lowest-RTT samples. Empty window uses the
    /// welcome fallback, or 0 when the guest has not heard from the host.
    pub fn offset_ms(&self) -> i64 {
        if self.samples.is_empty() {
            return self.fallback.unwrap_or(0);
        }
        let mut ranked = self.samples.clone();
        ranked.sort_by_key(|s| s.rtt_ms);
        let mut offs: Vec<i64> = ranked
            .iter()
            .take(ranked.len().min(Self::BEST))
            .map(|s| s.offset_ms)
            .collect();
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
    pub const THRESHOLD_MS: u64 = 250;
    pub const COOLDOWN: Duration = Duration::from_millis(1500);

    /// Forget the cooldown so the next meaningful error seeks immediately.
    /// Used after a track load or an explicit host seek.
    pub fn reset(&mut self) {
        self.last_seek = None;
    }

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
        // says 9000ms and stays in the FIFO window, but it must not vote:
        // only the lowest-RTT samples do.
        for _ in 0..8 {
            assert!(clock.observe(1_000, 2_100, 2_120, 1_200));
        }
        assert!(clock.observe(5_000, 14_500, 14_600, 5_800));
        assert_eq!(clock.samples.len(), 9);
        assert_eq!(clock.offset_ms(), 1_010);
        // (t4-t1) - (t3-t2) = 200 - 20 for the kept samples.
        assert_eq!(clock.best_rtt_ms(), Some(180));
        assert!(clock.est_host_now(10_000) > 10_000);
    }

    #[test]
    fn fallback_offset_covers_the_gap_before_the_first_pong() {
        let mut clock = ClockSync::default();
        assert_eq!(clock.offset_ms(), 0);
        clock.seed_fallback(15_000, 10_000);
        assert_eq!(clock.offset_ms(), 5_000);
        assert_eq!(clock.est_host_now(10_000), 15_000);
        assert!(clock.observe(1_000, 1_100, 1_120, 1_200));
        assert_eq!(clock.offset_ms(), 10);
    }

    #[test]
    fn fifo_window_follows_a_later_clock_step() {
        let mut clock = ClockSync::default();
        for i in 0..ClockSync::WINDOW {
            let t1 = (i as u64) * 1_000;
            // offset 1_000, rtt 100: t2 = t1+1050, t3 = t2+10, t4 = t1+110
            assert!(clock.observe(t1, t1 + 1_050, t1 + 1_060, t1 + 110));
        }
        assert_eq!(clock.offset_ms(), 1_000);
        for i in 0..ClockSync::WINDOW {
            let t1 = 100_000 + (i as u64) * 1_000;
            // offset 5_000, rtt 100.
            assert!(clock.observe(t1, t1 + 5_050, t1 + 5_060, t1 + 110));
        }
        assert_eq!(clock.samples.len(), ClockSync::WINDOW);
        assert_eq!(clock.offset_ms(), 5_000);
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
        gate.reset();
        assert!(
            gate.should_seek_at(800, t0 + DriftGate::COOLDOWN),
            "reset drops the cooldown for a load or an explicit seek"
        );
    }
}
