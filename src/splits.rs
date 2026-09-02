//! Split (per-act) detection from the LiveSplit splits panel.
//!
//! LiveSplit shows the PB-comparison time in rows not yet reached and the
//! actual time in completed rows — indistinguishable statically. So we detect
//! splits by CHANGE: snapshot every row at run start as the baseline; when a
//! row's value later changes (confirmed over consecutive reads, plausible
//! against the main timer, monotonic vs earlier acts), that act was just
//! completed and the new value is the real cumulative split.
//!
//! Known limitation: an act whose actual split lands within tolerance of its
//! PB-comparison value shows no change and goes unrecorded.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordedSplit {
    pub act_index: usize,
    pub act_name: String,
    pub cumulative_ms: i64,
}

pub struct SplitsTracker {
    tolerance_ms: i64,
    confirmations: usize,
    baseline: Vec<Option<i64>>,
    /// (candidate value, times seen consecutively)
    pending: Vec<Option<(i64, usize)>>,
    /// (last readable value, consecutive identical sightings) — used to
    /// backfill earlier acts once a later act proves them completed.
    stable: Vec<Option<(i64, usize)>>,
    recorded: Vec<Option<i64>>,
}

impl SplitsTracker {
    pub fn new(rows: usize, tolerance_ms: i64, confirmations: usize) -> Self {
        Self {
            tolerance_ms,
            confirmations: confirmations.max(1),
            baseline: vec![None; rows],
            pending: vec![None; rows],
            stable: vec![None; rows],
            recorded: vec![None; rows],
        }
    }

    /// One OCR pass over the panel: one value per row (None = unreadable).
    /// `timer_ms` is the current main-timer estimate, used as a plausibility
    /// bound (a split can't be later than "now"). Returns newly confirmed
    /// splits, in row order.
    pub fn observe(&mut self, values: &[Option<i64>], timer_ms: Option<i64>) -> Vec<(usize, i64)> {
        let mut out = Vec::new();
        for (i, v) in values.iter().enumerate().take(self.recorded.len()) {
            let Some(v) = *v else { continue }; // unreadable: keep pending as-is
            self.stable[i] = match self.stable[i] {
                Some((sv, n)) if (sv - v).abs() <= self.tolerance_ms => Some((sv, n + 1)),
                _ => Some((v, 1)),
            };
            if self.recorded[i].is_some() {
                continue;
            }
            let Some(base) = self.baseline[i] else {
                self.baseline[i] = Some(v);
                continue;
            };
            if (v - base).abs() <= self.tolerance_ms {
                self.pending[i] = None;
                continue;
            }
            // Row changed from its baseline: candidate split.
            let plausible = v > 0
                && timer_ms.map(|t| v <= t + 2500).unwrap_or(false)
                && self.recorded[..i]
                    .iter()
                    .flatten()
                    .all(|&earlier| v > earlier);
            if !plausible {
                self.pending[i] = None;
                continue;
            }
            let seen = match self.pending[i] {
                Some((pv, n)) if (pv - v).abs() <= self.tolerance_ms => n + 1,
                _ => 1,
            };
            if seen >= self.confirmations {
                // Act i is done — which proves every earlier act completed
                // too. Backfill any that never showed a visible change (an
                // actual time that TIES its PB comparison looks unchanged):
                // their rows now display the actual value, so a stable read
                // is trustworthy if it fits between its neighbors.
                for j in 0..i {
                    if self.recorded[j].is_some() {
                        continue;
                    }
                    let Some((sv, n)) = self.stable[j] else {
                        continue;
                    };
                    let after_prev = self.recorded[..j]
                        .iter()
                        .flatten()
                        .all(|&earlier| sv > earlier);
                    if n >= self.confirmations && sv > 0 && sv < v && after_prev {
                        self.recorded[j] = Some(sv);
                        self.pending[j] = None;
                        out.push((j, sv));
                    }
                }
                self.recorded[i] = Some(v);
                self.pending[i] = None;
                out.push((i, v));
            } else {
                self.pending[i] = Some((v, seen));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline row values (the PB comparison shown before each act is done).
    const PB: [i64; 3] = [50_000, 165_000, 250_000];

    fn seeded() -> SplitsTracker {
        let mut t = SplitsTracker::new(3, 100, 2);
        let base: Vec<Option<i64>> = PB.iter().copied().map(Some).collect();
        assert_eq!(t.observe(&base, Some(5_000)), vec![]);
        t
    }

    #[test]
    fn detects_confirmed_changes_in_order() {
        let mut t = seeded();
        // Act 1 completed at 48.2 (change appears, needs 2 sightings).
        let v = [Some(48_200), Some(165_000), Some(250_000)];
        assert_eq!(t.observe(&v, Some(55_000)), vec![]);
        assert_eq!(t.observe(&v, Some(60_000)), vec![(0, 48_200)]);
        // Act 2 later at 163.9.
        let v = [Some(48_200), Some(163_900), Some(250_000)];
        assert_eq!(t.observe(&v, Some(170_000)), vec![]);
        assert_eq!(t.observe(&v, Some(175_000)), vec![(1, 163_900)]);
        // No re-report.
        assert_eq!(t.observe(&v, Some(180_000)), vec![]);
    }

    #[test]
    fn one_off_misread_is_not_a_split() {
        let mut t = seeded();
        assert_eq!(t.observe(&[Some(43_000), None, None], Some(60_000)), vec![]);
        // Next read shows baseline again: candidate dropped.
        assert_eq!(t.observe(&[Some(50_000), None, None], Some(65_000)), vec![]);
        // A different bogus value doesn't inherit the old candidate's count.
        assert_eq!(t.observe(&[Some(44_500), None, None], Some(70_000)), vec![]);
        assert_eq!(t.observe(&[Some(50_000), None, None], Some(75_000)), vec![]);
    }

    #[test]
    fn implausible_values_rejected() {
        let mut t = seeded();
        // "Split" later than the current timer (wrong-scene static text).
        let v = [Some(48_200), None, None];
        assert_eq!(t.observe(&v, Some(20_000)), vec![]);
        assert_eq!(t.observe(&v, Some(21_000)), vec![]);
        // Non-monotonic vs an earlier recorded act.
        let mut t = seeded();
        let v = [Some(48_200), None, None];
        t.observe(&v, Some(55_000));
        t.observe(&v, Some(56_000)); // act 0 recorded at 48.2
        let bad = [Some(48_200), Some(30_000), None]; // act 1 "before" act 0
        assert_eq!(t.observe(&bad, Some(60_000)), vec![]);
        assert_eq!(t.observe(&bad, Some(61_000)), vec![]);
    }

    #[test]
    fn unreadable_rows_preserve_pending_confirmation() {
        let mut t = seeded();
        let v = [Some(48_200), None, None];
        assert_eq!(t.observe(&v, Some(55_000)), vec![]);
        // A fully unreadable pass shouldn't reset the candidate.
        assert_eq!(t.observe(&[None, None, None], Some(56_000)), vec![]);
        assert_eq!(t.observe(&v, Some(57_000)), vec![(0, 48_200)]);
    }

    #[test]
    fn act_tying_its_comparison_is_backfilled_by_the_next_act() {
        // Act 1's actual exactly equals the PB comparison: no visible change.
        // When Act 2 records, Act 1 must have happened — backfill it at its
        // stable displayed value.
        let mut t = seeded();
        // several reads with row 0 unchanged (stable), then act 2 changes
        let v = [Some(50_000), Some(163_900), Some(250_000)];
        assert_eq!(t.observe(&v, Some(170_000)), vec![]);
        assert_eq!(
            t.observe(&v, Some(175_000)),
            vec![(0, 50_000), (1, 163_900)]
        );
    }

    #[test]
    fn two_acts_can_confirm_in_one_pass() {
        let mut t = seeded();
        let v = [Some(48_200), Some(163_900), Some(250_000)];
        assert_eq!(t.observe(&v, Some(170_000)), vec![]);
        assert_eq!(
            t.observe(&v, Some(175_000)),
            vec![(0, 48_200), (1, 163_900)]
        );
    }

    #[test]
    fn baseline_captured_per_row_on_first_sight() {
        let mut t = SplitsTracker::new(2, 100, 2);
        // Row 1 unreadable at first: its baseline arrives later.
        assert_eq!(t.observe(&[Some(50_000), None], Some(5_000)), vec![]);
        assert_eq!(
            t.observe(&[Some(50_000), Some(165_000)], Some(6_000)),
            vec![]
        );
        let v = [Some(50_000), Some(160_000)];
        assert_eq!(t.observe(&v, Some(170_000)), vec![]);
        // Act 1 records; act 0 (which never visibly changed — a tie) is
        // backfilled at its stable displayed value, in order.
        assert_eq!(
            t.observe(&v, Some(171_000)),
            vec![(0, 50_000), (1, 160_000)]
        );
    }
}
