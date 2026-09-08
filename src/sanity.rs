//! Sanity on what the timer reader hands back.
//!
//! [`Smoother`] maintains a "current time" estimate from the last N accepted
//! readings, so a single late or slightly-off read doesn't wobble the
//! reported time. [`Monotone`] is the other half: it decides whether a
//! reading could have come from the clock at all.

use std::collections::VecDeque;

pub struct Smoother {
    window: usize,
    /// (wall_ms, timer_ms) pairs for accepted readings.
    buf: VecDeque<(i64, i64)>,
}

impl Smoother {
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            buf: VecDeque::new(),
        }
    }

    pub fn push(&mut self, wall_ms: i64, timer_ms: i64) {
        self.buf.push_back((wall_ms, timer_ms));
        while self.buf.len() > self.window {
            self.buf.pop_front();
        }
    }

    /// Median of each stored reading projected forward to `now_ms` (a running
    /// timer advances 1:1 with wall time, so projection is just addition).
    pub fn current(&self, now_ms: i64) -> Option<i64> {
        if self.buf.is_empty() {
            return None;
        }
        let mut proj: Vec<i64> = self.buf.iter().map(|&(t, v)| v + (now_ms - t)).collect();
        proj.sort_unstable();
        Some(proj[proj.len() / 2])
    }
}

/// How far outside what the clock could have done a reading may still sit
/// and be believed. A frame read late, or a hundredths pair caught mid-tick,
/// is worth a second; nothing legitimate is worth more.
const SLACK_MS: i64 = 1000;

/// Readings that must agree with each other, and disagree with what is held,
/// before what is held is abandoned. A clock started again reads a run like
/// that on its next few frames; wreckage does not.
const REANCHOR: u32 = 3;

/// A clock that only ever counts up, read through OCR.
///
/// The marathon total is such a clock: it starts at zero, pauses between
/// games and never resets for the length of an event. The READING of it is
/// not. When the big timer goes illegible tesseract does not stop answering
/// — it parses the wreckage, and the wreckage parses: "0499", "5.058",
/// "9.699" on consecutive frames of one real broadcast, minutes or hours
/// from the truth and different every frame. A total like that vetoes every
/// completion left in the day, or waves one through.
///
/// So a reading is believed only where the clock could have produced it: no
/// earlier than the last reading believed, and no further ahead of it than
/// the wall clock has moved. A reading that could not is no opinion at all —
/// which is the right answer, because the board's own arithmetic can speak
/// for a completion where the timer cannot, and a wrong total is worse than
/// no total.
///
/// This is deliberately not [`Smoother`]. That projects its readings forward
/// 1:1 with wall time, which is what a RUNNING timer does; a marathon total
/// stands still for as long as the runner takes to set the next game up, and
/// a smoothed one would run away from it.
///
/// What must not happen is holding a bad reading for ever. The timer really
/// does start again — a new game, a new day — and the first reading of it is
/// hours "backwards". So a run of readings that contradict what is held but
/// not each other, and that have moved on between the first and the last,
/// replaces it: a clock counting up does that within a few frames, and a
/// dead timer parsed to the same wrong number every frame never does.
/// One reading: when it was taken, and what it said.
type Reading = (i64, i64);

/// A run of readings that contradict what is held but not each other: the
/// first of them, the last, and how many.
type Chain = (Reading, Reading, u32);

#[derive(Debug, Default)]
pub struct Monotone {
    /// The last reading believed.
    held: Option<Reading>,
    chain: Option<Chain>,
}

/// Could the clock have gone from one reading to the other?
fn reachable(from: Reading, to: Reading) -> bool {
    let ahead = to.1 - from.1;
    ahead >= -SLACK_MS && ahead <= (to.0 - from.0).max(0) + SLACK_MS
}

impl Monotone {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer one reading. Returns it when it is believed, and None when the
    /// clock could not have produced it.
    pub fn push(&mut self, wall_ms: i64, value_ms: i64) -> Option<i64> {
        let now = (wall_ms, value_ms);
        if self.held.is_none_or(|h| reachable(h, now)) {
            self.held = Some(now);
            self.chain = None;
            return Some(value_ms);
        }
        let (first, last, n) = match self.chain {
            Some((first, last, n)) if reachable(last, now) => (first, now, n + 1),
            _ => (now, now, 1),
        };
        self.chain = Some((first, last, n));
        if n >= REANCHOR && last.1 > first.1 {
            self.held = Some(now);
            self.chain = None;
            return Some(value_ms);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that counts up is believed; one that goes backwards, or
    /// faster than the wall clock, is not.
    #[test]
    fn a_reading_the_clock_could_not_have_made_is_no_opinion() {
        let mut m = Monotone::new();
        assert_eq!(m.push(0, 0), Some(0));
        assert_eq!(m.push(1000, 1000), Some(1000));
        // The marathon total pauses between games: the same value, for
        // minutes, is exactly what it should read.
        assert_eq!(m.push(60_000, 1000), Some(1000));
        assert_eq!(m.push(120_000, 1000), Some(1000));
        // And then counts on again from where it stood.
        assert_eq!(m.push(180_000, 61_000), Some(61_000));
        // Backwards is not a thing an event total does.
        assert_eq!(m.push(181_000, 5_058), None);
        // Nor is an hour in a second.
        assert_eq!(m.push(182_000, 3_600_000), None);
        // The reading that is right is still believed afterwards.
        assert_eq!(m.push(183_000, 64_000), Some(64_000));
    }

    /// The wreckage a dead timer parses to, in the order one real broadcast
    /// produced it. None of it agrees with the clock, and none of it agrees
    /// with the rest, so none of it is believed however long it goes on.
    #[test]
    fn wreckage_never_becomes_the_clock() {
        let mut m = Monotone::new();
        assert_eq!(m.push(0, 6_174_000), Some(6_174_000)); // 1:42:54
        for t in 1..40 {
            for v in [5_058, 155_888_000, 3_558_020, 9_699, 499] {
                assert_eq!(m.push(t * 1000, v), None, "at {t}: {v}");
            }
        }
        // A timer stuck on one wrong number is not a clock either: it has
        // not moved, so it never replaces what is held.
        for t in 40..80 {
            assert_eq!(m.push(t * 1000, 4_990), None, "at {t}");
        }
    }

    /// The timer really does start again — a new game, a new day — and the
    /// first reading of it is hours behind. A few readings that agree with
    /// each other and have moved on replace what is held; until then the
    /// gate says nothing rather than the wrong thing.
    #[test]
    fn a_clock_that_started_again_is_picked_up() {
        let mut m = Monotone::new();
        assert_eq!(m.push(0, 12_000_000), Some(12_000_000));
        assert_eq!(m.push(1000, 30), None);
        assert_eq!(m.push(2000, 1_030), None);
        assert_eq!(m.push(3000, 2_030), Some(2_030));
        assert_eq!(m.push(4000, 3_030), Some(3_030));
    }

    #[test]
    fn empty_gives_none() {
        assert_eq!(Smoother::new(5).current(1000), None);
    }

    #[test]
    fn projects_readings_forward() {
        let mut s = Smoother::new(5);
        s.push(0, 1000);
        s.push(1000, 2000);
        s.push(2000, 3000);
        // All three project to 4000 at t=3000.
        assert_eq!(s.current(3000), Some(4000));
    }

    #[test]
    fn median_absorbs_one_late_read() {
        let mut s = Smoother::new(5);
        s.push(0, 1000);
        s.push(1000, 2000);
        s.push(2000, 2600); // read arrived late: projects 400ms low
        assert_eq!(s.current(3000), Some(4000));
    }

    #[test]
    fn window_evicts_oldest() {
        let mut s = Smoother::new(2);
        s.push(0, 100_000); // stale, should be evicted
        s.push(1000, 1000);
        s.push(2000, 2000);
        assert_eq!(s.current(2000), Some(2000));
    }
}
