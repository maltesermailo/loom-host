//! Clock synchronization min-filter — PROTOCOL.md §7.
//!
//! From each PING/PONG exchange with client times `t0` (send) and `t3` (PONG
//! receive) and host times `t1`, `t2`:
//! ```text
//! rtt    = (t3 − t0) − (t2 − t1)
//! offset = floor(((t1 − t0) + (t2 − t3)) / 2)      // host ≈ client + offset
//! ```
//! All arithmetic is signed 64-bit microseconds; the division floors toward
//! negative infinity. The current estimate is the `(rtt, offset)` of the
//! **minimum-rtt** sample over the last 16 samples, ties won by the more recent
//! sample.

/// Sliding-window size (§7).
pub const WINDOW: usize = 16;

/// A clock-offset/RTT estimate in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Round-trip time, µs.
    pub rtt: i64,
    /// `host_time ≈ client_time + offset`, µs.
    pub offset: i64,
}

/// The min-rtt sliding-window filter.
#[derive(Clone, Debug, Default)]
pub struct ClockFilter {
    window: Vec<Estimate>,
}

impl ClockFilter {
    /// A fresh, empty filter.
    pub fn new() -> Self {
        Self { window: Vec::new() }
    }

    /// Incorporate one PING/PONG sample and return the current estimate.
    ///
    /// `t0`, `t3` are client-clock µs (ping send / pong receive); `t1`, `t2` are
    /// host-clock µs (host receive / host send).
    pub fn push(&mut self, t0: i64, t1: i64, t2: i64, t3: i64) -> Estimate {
        let rtt = (t3 - t0) - (t2 - t1);
        // floor(((t1 - t0) + (t2 - t3)) / 2), rounding toward negative infinity.
        let offset = ((t1 - t0) + (t2 - t3)).div_euclid(2);

        self.window.push(Estimate { rtt, offset });
        if self.window.len() > WINDOW {
            self.window.remove(0);
        }

        // Min rtt over the window; on a tie the more recent (later) sample wins,
        // which the `<=` comparison over the oldest→newest order achieves.
        let mut best = self.window[0];
        for s in &self.window[1..] {
            if s.rtt <= best.rtt {
                best = *s;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_arithmetic() {
        let mut f = ClockFilter::new();
        let e = f.push(1000, 501_500, 501_540, 2100);
        assert_eq!(e.rtt, 1060);
        assert_eq!(e.offset, 499_970);
    }

    #[test]
    fn min_rtt_becomes_best() {
        let mut f = ClockFilter::new();
        f.push(1000, 501_500, 501_540, 2100); // rtt 1060
        let e2 = f.push(501_000, 1_002_200, 1_002_240, 502_400); // rtt 1360
        assert_eq!(e2.rtt, 1060); // previous min still wins
        let e3 = f.push(1_001_000, 1_501_900, 1_501_940, 1_002_000); // rtt 960
        assert_eq!(e3.rtt, 960);
        assert_eq!(e3.offset, 500_420);
    }

    #[test]
    fn negative_offset_floors() {
        let mut f = ClockFilter::new();
        let e = f.push(0, -250_000, -249_960, 1000);
        assert_eq!(e.rtt, 960);
        assert_eq!(e.offset, -250_480);
    }

    #[test]
    fn floor_division_rounds_toward_negative_infinity() {
        // Construct a sum that is odd and negative: ((t1-t0)+(t2-t3)) = -3 -> -2.
        let mut f = ClockFilter::new();
        let e = f.push(0, 0, 0, 3); // (0-0)+(0-3) = -3, floor(-1.5) = -2
        assert_eq!(e.offset, -2);
    }

    #[test]
    fn tie_breaks_to_more_recent_sample() {
        let mut f = ClockFilter::new();
        // Two samples with equal rtt but different offsets; the later wins.
        f.push(0, 1000, 1040, 100); // rtt = (100-0)-(1040-1000) = 60
        let e = f.push(0, 2000, 2040, 100); // rtt = 60 as well, offset differs
        assert_eq!(e.rtt, 60);
        // offset of the second sample: ((2000-0)+(2040-100))/2 = (2000+1940)/2 = 1970
        assert_eq!(e.offset, 1970);
    }

    #[test]
    fn window_evicts_old_minimum() {
        let mut f = ClockFilter::new();
        // First sample has a very low rtt; then 16 samples with higher rtt push
        // it out of the 16-window, so the estimate must degrade.
        let e0 = f.push(0, 500, 540, 100); // rtt = (100-0)-(540-500) = 60
        assert_eq!(e0.rtt, 60);
        for i in 0..WINDOW {
            let base = 1_000_000 + (i as i64) * 1000;
            f.push(base, base + 500, base + 600, base + 200);
            // rtt = (200)-(600-500) = 100
        }
        // The low-rtt sample has been evicted; best remaining rtt is 100.
        let e = f.push(9_000_000, 9_000_500, 9_000_600, 9_000_200);
        assert_eq!(e.rtt, 100);
    }
}
