//! Derived statistics, computed at query time from raw run rows so that a
//! `!correct` or `!void` can never leave a cached aggregate stale.

use serde::Serialize;

/// The minimal slice of a run row the stats need, in insertion order.
#[derive(Debug, Clone)]
pub struct RunBrief {
    pub started_at_ms: i64,
    pub attempt_number: i64,
    pub finished: bool,
    pub final_time_ms: Option<i64>,
    pub last_timer_ms: Option<i64>,
}

/// An act with its cumulative end time; None = the final act (unbounded).
pub type Act = (String, Option<i64>);

#[derive(Debug, Clone, Serialize)]
pub struct DeathBucket {
    pub label: String,
    pub deaths: i64,
    pub pct: f64,
}

/// Where runs die: resets bucketed by act (using cumulative act-end times),
/// or by minute when no act boundaries are configured. Percentages are of
/// total resets.
pub fn death_chart(runs: &[RunBrief], acts: &[Act]) -> Vec<DeathBucket> {
    let deaths: Vec<i64> = runs
        .iter()
        .filter(|r| !r.finished)
        .filter_map(|r| r.last_timer_ms)
        .collect();
    if deaths.is_empty() {
        return Vec::new();
    }
    let total = deaths.len() as i64;
    let mut buckets: Vec<(String, i64)> = if acts.is_empty() {
        let max_min = deaths.iter().max().unwrap() / 60_000;
        let mut v: Vec<(String, i64)> = (0..=max_min)
            .map(|m| (format!("{m}-{}m", m + 1), 0))
            .collect();
        for d in &deaths {
            v[(d / 60_000) as usize].1 += 1;
        }
        v
    } else {
        let mut v: Vec<(String, i64)> = acts.iter().map(|(n, _)| (n.clone(), 0)).collect();
        for d in &deaths {
            let idx = acts
                .iter()
                .position(|(_, end)| end.map(|e| *d < e).unwrap_or(true))
                .unwrap_or(acts.len() - 1);
            v[idx].1 += 1;
        }
        v
    };
    // Trailing empty buckets are noise; internal empty ones are information.
    while buckets.last().map(|(_, c)| *c == 0).unwrap_or(false) {
        buckets.pop();
    }
    buckets
        .into_iter()
        .map(|(label, deaths)| DeathBucket {
            label,
            deaths,
            pct: deaths as f64 * 100.0 / total as f64,
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct PbPoint {
    pub at_ms: i64,
    pub attempt_number: i64,
    pub time_ms: i64,
}

/// Every time the personal best improved, in order.
pub fn pb_history(runs: &[RunBrief]) -> Vec<PbPoint> {
    let mut best: Option<i64> = None;
    let mut out = Vec::new();
    for r in runs {
        if let (true, Some(t)) = (r.finished, r.final_time_ms) {
            if best.map(|b| t < b).unwrap_or(true) {
                best = Some(t);
                out.push(PbPoint {
                    at_ms: r.started_at_ms,
                    attempt_number: r.attempt_number,
                    time_ms: t,
                });
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Streaks {
    pub attempts: i64,
    pub finished: i64,
    pub longest_reset_streak: i64,
    pub current_reset_streak: i64,
    /// Attempts per finished run, None until something finishes.
    pub attempts_per_finish: Option<f64>,
}

pub fn streaks(runs: &[RunBrief]) -> Streaks {
    let mut s = Streaks {
        attempts: runs.len() as i64,
        ..Default::default()
    };
    let mut cur = 0;
    for r in runs {
        if r.finished {
            s.finished += 1;
            cur = 0;
        } else {
            cur += 1;
            s.longest_reset_streak = s.longest_reset_streak.max(cur);
        }
    }
    s.current_reset_streak = cur;
    if s.finished > 0 {
        s.attempts_per_finish = Some(s.attempts as f64 / s.finished as f64);
    }
    s
}

/// Fraction of runs still alive at each act boundary: (act name, survived,
/// pct of attempts that got past its end). The final (unbounded) act is
/// reported as finishes.
pub fn survival(runs: &[RunBrief], acts: &[Act]) -> Vec<DeathBucket> {
    if runs.is_empty() || acts.is_empty() {
        return Vec::new();
    }
    let total = runs.len() as i64;
    let mut out = Vec::new();
    for (name, end) in acts {
        let survived = match end {
            Some(e) => runs
                .iter()
                .filter(|r| {
                    r.finished || r.last_timer_ms.map(|d| d >= *e).unwrap_or(false)
                })
                .count() as i64,
            None => runs.iter().filter(|r| r.finished).count() as i64,
        };
        out.push(DeathBucket {
            label: name.clone(),
            deaths: survived, // field reused as "survived count" here
            pct: survived as f64 * 100.0 / total as f64,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(n: i64, fin: Option<i64>, last: i64) -> RunBrief {
        RunBrief {
            started_at_ms: n * 1000,
            attempt_number: n,
            finished: fin.is_some(),
            final_time_ms: fin,
            last_timer_ms: Some(fin.unwrap_or(last)),
        }
    }

    fn acts() -> Vec<Act> {
        vec![
            ("Act 1".into(), Some(55_000)),
            ("Act 2".into(), Some(175_000)),
            ("Act 3".into(), None),
        ]
    }

    #[test]
    fn death_chart_buckets_by_act() {
        let runs = vec![
            run(1, None, 10_000),   // Act 1
            run(2, None, 54_999),   // Act 1
            run(3, None, 55_000),   // Act 2 (boundary is exclusive end)
            run(4, None, 600_000),  // Act 3 (unbounded)
            run(5, Some(700_000), 0), // finished — not a death
        ];
        let chart = death_chart(&runs, &acts());
        assert_eq!(chart.len(), 3);
        assert_eq!((chart[0].label.as_str(), chart[0].deaths), ("Act 1", 2));
        assert_eq!(chart[1].deaths, 1);
        assert_eq!(chart[2].deaths, 1);
        assert!((chart[0].pct - 50.0).abs() < 1e-9);
    }

    #[test]
    fn death_chart_minute_fallback_and_trailing_trim() {
        let runs = vec![run(1, None, 30_000), run(2, None, 45_000), run(3, None, 130_000)];
        let chart = death_chart(&runs, &[]);
        assert_eq!(chart.len(), 3); // 0-1m, 1-2m (empty, kept), 2-3m
        assert_eq!(chart[0].deaths, 2);
        assert_eq!(chart[1].deaths, 0);
        assert_eq!(chart[2].deaths, 1);
    }

    #[test]
    fn death_chart_empty_without_resets() {
        assert!(death_chart(&[run(1, Some(700_000), 0)], &acts()).is_empty());
        assert!(death_chart(&[], &acts()).is_empty());
    }

    #[test]
    fn pb_history_tracks_improvements_only() {
        let runs = vec![
            run(1, None, 10_000),
            run(2, Some(720_000), 0),
            run(3, Some(730_000), 0), // slower — not a PB
            run(4, Some(700_000), 0),
        ];
        let h = pb_history(&runs);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].time_ms, 720_000);
        assert_eq!(h[1].time_ms, 700_000);
        assert_eq!(h[1].attempt_number, 4);
    }

    #[test]
    fn streaks_and_rates() {
        let runs = vec![
            run(1, None, 1),
            run(2, None, 1),
            run(3, None, 1),
            run(4, Some(700_000), 0),
            run(5, None, 1),
        ];
        let s = streaks(&runs);
        assert_eq!(s.attempts, 5);
        assert_eq!(s.finished, 1);
        assert_eq!(s.longest_reset_streak, 3);
        assert_eq!(s.current_reset_streak, 1);
        assert_eq!(s.attempts_per_finish, Some(5.0));
    }

    #[test]
    fn survival_counts_runs_past_each_boundary() {
        let runs = vec![
            run(1, None, 10_000),     // died Act 1
            run(2, None, 100_000),    // died Act 2
            run(3, Some(700_000), 0), // finished
        ];
        let s = survival(&runs, &acts());
        assert_eq!(s[0].deaths, 2); // got past Act 1: the 100k death + finisher
        assert_eq!(s[1].deaths, 1); // past Act 2: finisher only
        assert_eq!(s[2].deaths, 1); // finished
    }
}
