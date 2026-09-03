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
//!   still on screen, or a misread of the current one (a 9 read as a 1). Only
//!   when TWO consecutive runs each settle on a lower value, the second above
//!   the first, is the adopted value questioned — one run's worth of reads,
//!   however many, is one static display. Then:
//!   - if that lower sequence continues from the value adopted BEFORE the
//!     questioned one, the questioned one was the misread: revert, and clear
//!     it wherever it was recorded;
//!   - if the lower values are a fraction of it, the streamer reset his
//!     counter (a new splits file): adopt the new numbering, clear nothing;
//!   - otherwise it is a systematic misread of the current numbers, and the
//!     adopted value stands.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterEvent {
    /// Nothing to do with this reading.
    Ignore,
    /// Give the current run this number.
    Adopt(i64),
    /// The number previously adopted was a misread; `bogus` must be cleared
    /// wherever it was recorded and the current run gets `to`.
    Revert { bogus: i64, to: i64 },
    /// The counter itself restarted (a new splits file): the current run gets
    /// this number and earlier rows are left as they are.
    Rebase(i64),
}

#[derive(Debug, Clone)]
pub struct CounterTracker {
    last: Option<i64>,
    /// When `last` was adopted this session; None when it was seeded from
    /// the database at startup (possibly days old).
    last_at: Option<i64>,
    /// The value adopted before `last`.
    before_last: Option<i64>,
    /// (candidate above `last`, consecutive sightings)
    stable: Option<(i64, u32)>,
    /// (value below `last` in the current run, consecutive sightings)
    lower: Option<(i64, u32)>,
    /// The lower value each recent run settled on (seen twice), oldest
    /// first — at most one per run; cleared whenever a value above `last`
    /// is adopted.
    lower_runs: Vec<i64>,
    /// This run has already contributed its settled lower value.
    lower_settled: bool,
}

impl CounterTracker {
    pub fn new(seed: Option<i64>) -> Self {
        Self {
            last: seed,
            last_at: None,
            before_last: None,
            stable: None,
            lower: None,
            lower_runs: Vec::new(),
            lower_settled: false,
        }
    }

    #[cfg(test)]
    pub fn last(&self) -> Option<i64> {
        self.last
    }

    /// A new run began: the display may still show the old number, so no
    /// streak carries over, and this run may settle on one lower value.
    pub fn reset_run(&mut self) {
        self.stable = None;
        self.lower = None;
        self.lower_settled = false;
    }

    fn adopt(&mut self, v: i64, t_ms: i64) {
        self.before_last = self.last;
        self.last = Some(v);
        self.last_at = Some(t_ms);
        self.stable = None;
        self.lower = None;
        self.lower_runs.clear();
        self.lower_settled = false;
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
                // This run settles on one lower value, once.
                if !self.lower_settled && matches!(self.lower, Some((_, n)) if n >= 2) {
                    self.lower_settled = true;
                    self.lower_runs.push(v);
                    if self.lower_runs.len() > 4 {
                        self.lower_runs.remove(0);
                    }
                }
                if let [.., a, b] = self.lower_runs[..] {
                    if b > a && b < p {
                        // Continuing from before the questioned value: that
                        // value was the misread.
                        if self.before_last.is_some_and(|bl| a > bl) {
                            self.adopt(b, t_ms);
                            return CounterEvent::Revert { bogus: p, to: b };
                        }
                        // A fraction of it: the streamer's counter restarted.
                        if b < p / 2 {
                            self.adopt(b, t_ms);
                            return CounterEvent::Rebase(b);
                        }
                        // Otherwise a systematic misread of the current
                        // numbers (a 9 read as a 1 on run after run); the
                        // adopted value stands, and the runs stay unnumbered
                        // for fill-run-numbers to infer.
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
                    self.adopt(v, t_ms);
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

    fn adopt(c: &mut CounterTracker, v: i64, t: i64) {
        c.reset_run();
        for k in 0..3 {
            c.observe(v, t + 2_000 * k);
        }
        assert_eq!(c.last(), Some(v));
    }

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
        adopt(&mut c, 95_049, 0);
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
        // The database's last number is 95040; the session's first read is
        // 95949, a misread nothing can be checked against (no jump limit for
        // the first value). The next run reads 95050, the one after 95051:
        // the real sequence, carrying on from before the bogus value.
        let mut c = CounterTracker::new(Some(95_040));
        adopt(&mut c, 95_949, 3_600_000);
        c.reset_run();
        for t in [3_660_000, 3_662_000, 3_664_000, 3_666_000] {
            assert_eq!(
                c.observe(95_050, t),
                CounterEvent::Ignore,
                "one run is not enough"
            );
        }
        c.reset_run();
        assert_eq!(c.observe(95_051, 3_720_000), CounterEvent::Ignore);
        assert_eq!(
            c.observe(95_051, 3_722_000),
            CounterEvent::Revert {
                bogus: 95_949,
                to: 95_051
            }
        );
        assert_eq!(c.last(), Some(95_051));
        // The previous run's number still on screen is not a revert.
        c.reset_run();
        for t in [3_730_000, 3_732_000, 3_734_000, 3_736_000] {
            assert_eq!(c.observe(95_051, t), CounterEvent::Ignore);
        }
        assert_eq!(c.observe(95_052, 3_738_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_052, 3_740_000), CounterEvent::Adopt(95_052));
    }

    #[test]
    fn two_lower_values_inside_one_run_do_not_revert() {
        // 95050 is right. Within ONE run the display misreads 95030 twice,
        // then 95040 twice: a run contributes one settled value, not two.
        let mut c = CounterTracker::new(None);
        adopt(&mut c, 95_049, 0);
        adopt(&mut c, 95_050, 60_000);
        c.reset_run();
        for (v, t) in [
            (95_030, 70_000),
            (95_030, 72_000),
            (95_040, 74_000),
            (95_040, 76_000),
        ] {
            assert_eq!(c.observe(v, t), CounterEvent::Ignore);
        }
        assert_eq!(c.last(), Some(95_050));
        // Same with an upper read in between.
        for (v, t) in [(95_035, 78_000), (95_035, 80_000)] {
            assert_eq!(c.observe(v, t), CounterEvent::Ignore);
        }
        assert_eq!(c.last(), Some(95_050));
    }

    #[test]
    fn a_systematic_downward_misread_does_not_revert_a_correct_number() {
        // 95048 then 95049 were adopted correctly. A 9 read as a 1 makes the
        // next two runs settle on 95041 and 95042: increasing and lower, but
        // NOT continuing from 95048 — the adopted value stands.
        let mut c = CounterTracker::new(None);
        adopt(&mut c, 95_048, 0);
        adopt(&mut c, 95_049, 60_000);
        c.reset_run();
        assert_eq!(c.observe(95_041, 120_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_041, 122_000), CounterEvent::Ignore);
        c.reset_run();
        assert_eq!(c.observe(95_042, 180_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_042, 182_000), CounterEvent::Ignore);
        assert_eq!(c.last(), Some(95_049));
        // The true next number is still adopted normally.
        c.reset_run();
        assert_eq!(c.observe(95_052, 240_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_052, 242_000), CounterEvent::Adopt(95_052));
    }

    #[test]
    fn a_counter_reset_by_the_streamer_rebases_without_clearing() {
        let mut c = CounterTracker::new(None);
        adopt(&mut c, 95_049, 0);
        adopt(&mut c, 95_050, 60_000);
        // New splits file: the counter starts over at 1, 2, ...
        c.reset_run();
        c.observe(1, 120_000);
        assert_eq!(c.observe(1, 122_000), CounterEvent::Ignore);
        c.reset_run();
        c.observe(2, 180_000);
        assert_eq!(c.observe(2, 182_000), CounterEvent::Rebase(2));
        assert_eq!(c.last(), Some(2));
        c.reset_run();
        assert_eq!(c.observe(3, 240_000), CounterEvent::Ignore);
        assert_eq!(c.observe(3, 242_000), CounterEvent::Adopt(3));
    }

    #[test]
    fn a_gap_of_unseen_short_attempts_is_allowed() {
        let mut c = CounterTracker::new(None);
        adopt(&mut c, 95_000, 0);
        c.reset_run();
        // Ten quick resets we never saw, over two minutes: 5 + 120/10 = 17 allowed.
        assert_eq!(c.observe(95_011, 120_000), CounterEvent::Ignore);
        assert_eq!(c.observe(95_011, 122_000), CounterEvent::Adopt(95_011));
    }
}
