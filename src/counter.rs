//! LiveSplit's attempt counter, read off the layout: which value to believe.
//!
//! The counter only ever grows, by one per attempt, and the display is static
//! text — so a misread repeats identically on every read, and "seen N times"
//! is weak evidence on its own. What the tracker knows instead:
//!
//! - within a session, attempts cannot arrive faster than one every few
//!   seconds, so a value far ahead of the last one adopted is a misread (a 0
//!   read as a 9 turns 95049 into 95949) and is refused outright;
//! - a value BELOW the one adopted is normally the previous attempt's number
//!   still on screen, or a misread of the current one (a 9 read as a 1). But
//!   if TWO consecutive runs each settle on a lower value, and the second is
//!   above the first — the sequence carrying on from before the adopted
//!   value — then the adopted one was the misread, and it is reverted, or
//!   every real number after it would be locked out for the rest of the day.
//!   One run's worth of identical lower reads is not enough: a static display
//!   repeats its misread on every read.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterEvent {
    /// Nothing to do with this reading.
    Ignore,
    /// Give the current run this number.
    Adopt(i64),
    /// The number previously adopted was a misread; `bogus` must be cleared
    /// wherever it was recorded and the current run gets `to`.
    Revert { bogus: i64, to: i64 },
}

#[derive(Debug, Clone)]
pub struct CounterTracker {
    last: Option<i64>,
    /// When `last` was adopted this session; None when it was seeded from
    /// the database at startup (possibly days old).
    last_at: Option<i64>,
    /// (candidate above `last`, consecutive sightings)
    stable: Option<(i64, u32)>,
    /// (value below `last` in the current run, consecutive sightings)
    lower: Option<(i64, u32)>,
    /// The lower value each recent run settled on (seen twice), oldest
    /// first; cleared whenever a value above `last` is adopted.
    lower_runs: Vec<i64>,
}

impl CounterTracker {
    pub fn new(seed: Option<i64>) -> Self {
        Self {
            last: seed,
            last_at: None,
            stable: None,
            lower: None,
            lower_runs: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn last(&self) -> Option<i64> {
        self.last
    }

    /// A new run began: the display may still show the old number, so no
    /// streak carries over.
    pub fn reset_run(&mut self) {
        self.stable = None;
        self.lower = None;
    }

    /// One parsed reading of the counter at time `t_ms`.
    pub fn observe(&mut self, v: i64, t_ms: i64) -> CounterEvent {
        match self.last {
            Some(p) if v <= p => {
                if v == p {
                    return CounterEvent::Ignore; // the current number, still on screen
                }
                self.lower = match self.lower {
                    Some((pv, n)) if pv == v => Some((v, n + 1)),
                    _ => Some((v, 1)),
                };
                // This run has settled on a lower value: remember it once.
                if matches!(self.lower, Some((_, 2))) {
                    self.lower_runs.push(v);
                }
                // Two runs in a row below the adopted value, the later one
                // higher: the sequence is continuing from before it.
                if let [.., a, b] = self.lower_runs[..] {
                    if b > a && b < p {
                        self.last = Some(b);
                        self.last_at = Some(t_ms);
                        self.stable = None;
                        self.lower = None;
                        self.lower_runs.clear();
                        return CounterEvent::Revert { bogus: p, to: b };
                    }
                }
                CounterEvent::Ignore
            }
            _ => {
                self.lower = None;
                // One attempt per ten seconds at the very most, plus slack for
                // attempts that were too short to see.
                if let (Some(p), Some(at)) = (self.last, self.last_at) {
                    if v - p > 5 + (t_ms - at) / 10_000 {
                        return CounterEvent::Ignore;
                    }
                }
                // Nothing vouches for the first value of a session (the seed
                // may be days old): it needs three identical reads.
                let need = if self.last_at.is_some() { 2 } else { 3 };
                self.stable = match self.stable {
                    Some((pv, n)) if pv == v => Some((v, n + 1)),
                    _ => Some((v, 1)),
                };
                if matches!(self.stable, Some((_, n)) if n >= need) {
                    self.last = Some(v);
                    self.last_at = Some(t_ms);
                    self.lower_runs.clear();
                    return CounterEvent::Adopt(v);
                }
                CounterEvent::Ignore
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_of_a_session_needs_three_reads_then_two() {
        let mut c = CounterTracker::new(Some(95_000)); // from the database
        assert_eq!(c.observe(95_026, 0), CounterEvent::Ignore);
        assert_eq!(c.observe(95_026, 2_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_026, 4_000), CounterEvent::Adopt(95_026));
        c.reset_run();
        assert_eq!(c.observe(95_027, 60_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_027, 62_000), CounterEvent::Adopt(95_027));
    }

    #[test]
    fn a_zero_read_as_nine_is_refused_outright() {
        let mut c = CounterTracker::new(None);
        for t in [0, 2_000, 4_000] {
            c.observe(95_049, t);
        }
        assert_eq!(c.last(), Some(95_049));
        c.reset_run();
        // 95949 three times, 30 seconds later: 900 attempts in 30 seconds
        // is impossible, so it never becomes stable.
        for t in [30_000, 32_000, 34_000, 36_000] {
            assert_eq!(c.observe(95_949, t), CounterEvent::Ignore);
        }
        assert_eq!(c.last(), Some(95_049));
        // ...and the real next number is still accepted.
        assert_eq!(c.observe(95_050, 40_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_050, 42_000), CounterEvent::Adopt(95_050));
    }

    #[test]
    fn a_bogus_first_value_is_reverted_by_two_runs_continuing_below_it() {
        // The session's first value was misread as 95949 (nothing to compare
        // it with). The next run reads 95050, the one after 95051: the real
        // sequence, carrying on from before the bogus value.
        let mut c = CounterTracker::new(Some(94_000));
        for t in [0, 2_000, 4_000] {
            c.observe(95_949, t);
        }
        assert_eq!(c.last(), Some(95_949));
        c.reset_run();
        for t in [60_000, 62_000, 64_000, 66_000] {
            assert_eq!(
                c.observe(95_050, t),
                CounterEvent::Ignore,
                "one run is not enough"
            );
        }
        c.reset_run();
        assert_eq!(c.observe(95_051, 120_000), CounterEvent::Ignore);
        assert_eq!(
            c.observe(95_051, 122_000),
            CounterEvent::Revert {
                bogus: 95_949,
                to: 95_051
            }
        );
        assert_eq!(c.last(), Some(95_051));
        // The previous run's number still on screen is not a revert.
        c.reset_run();
        for t in [130_000, 132_000, 134_000, 136_000] {
            assert_eq!(c.observe(95_051, t), CounterEvent::Ignore);
        }
        assert_eq!(c.observe(95_052, 138_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_052, 140_000), CounterEvent::Adopt(95_052));
    }

    #[test]
    fn a_misread_of_the_current_number_does_not_revert_it() {
        // 95049 is right. A 9 read as a 1 makes the display say 95041 on
        // every read of this run — three, four, five times — and the next
        // run reads its true number 95050. Nothing is reverted.
        let mut c = CounterTracker::new(None);
        for t in [0, 2_000, 4_000] {
            c.observe(95_049, t);
        }
        c.reset_run();
        for t in [10_000, 12_000, 14_000, 16_000, 18_000] {
            assert_eq!(c.observe(95_041, t), CounterEvent::Ignore);
        }
        assert_eq!(c.last(), Some(95_049));
        c.reset_run();
        assert_eq!(c.observe(95_050, 60_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_050, 62_000), CounterEvent::Adopt(95_050));
        // Two runs of lower misreads that do NOT continue a sequence (the
        // second is not above the first) do not revert either.
        c.reset_run();
        c.observe(95_045, 70_000);
        c.observe(95_045, 72_000);
        c.reset_run();
        c.observe(95_043, 80_000);
        assert_eq!(c.observe(95_043, 82_000), CounterEvent::Ignore);
        assert_eq!(c.last(), Some(95_050));
    }

    #[test]
    fn a_gap_of_unseen_short_attempts_is_allowed() {
        let mut c = CounterTracker::new(None);
        for t in [0, 2_000, 4_000] {
            c.observe(95_000, t);
        }
        c.reset_run();
        // Ten quick resets we never saw, over two minutes: 5 + 120/10 = 17 allowed.
        assert_eq!(c.observe(95_011, 120_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_011, 122_000), CounterEvent::Adopt(95_011));
    }
}
