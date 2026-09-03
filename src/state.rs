//! Run-state machine: consumes one timer observation per captured frame and
//! emits run lifecycle events.
//!
//! ```text
//! IDLE    -> RUNNING   timer left ~0:00 and advanced consistently for N readings
//! RUNNING -> IDLE      via Finished (timer legible but frozen for N readings)
//!                      via Reset:
//!                        Zeroed      timer back at ~0:00
//!                        Disappeared OCR failed for a long stretch while frames
//!                                    kept arriving (timer removed / runner quit)
//!                        Desync      readings stopped matching the running clock
//!                                    but are self-consistent -> we missed a reset;
//!                                    re-sync onto the new run immediately
//! ```
//!
//! All timestamps are milliseconds on a monotonic clock supplied by the
//! caller, which keeps the machine fully deterministic for tests. Validation
//! of "did the timer advance by roughly the frame interval" is done against
//! *elapsed wall time*, not an assumed interval, so the machine self-heals
//! across stream drops and ad breaks: when frames resume, the timer has
//! advanced by exactly as much as the wall clock and the reading is accepted.

use serde::Deserialize;

use crate::sanity::Smoother;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrackerConfig {
    /// Timer values at or below this count as "at zero".
    pub start_epsilon_ms: i64,
    /// Consecutive advancing readings required to declare a run started.
    pub start_confirmations: usize,
    /// Minimum forward progress between two readings to count as advancing.
    pub min_advance_ms: i64,
    /// How far (delta vs elapsed wall time) two readings may disagree and
    /// still count as consistent advancement.
    pub advance_slack_ms: i64,
    /// Readings within this of the previous value count as "not advancing".
    pub stall_tolerance_ms: i64,
    /// Consecutive non-advancing (legible) readings to declare a finish.
    pub stall_confirmations: usize,
    /// Readings below this count as "returned to zero" while running.
    pub reset_epsilon_ms: i64,
    /// Consecutive near-zero readings required to declare a reset.
    pub reset_confirmations: usize,
    /// Consecutive illegible frames (stream still live!) to declare the timer
    /// gone and the run dead. At 1 fps this is seconds. Keep it generous:
    /// mid-roll ads can blank the feed for a couple of minutes.
    pub illegible_reset_count: usize,
    /// Readings deviating from the expected value by more than this are
    /// misreads (unless near zero) and are skipped.
    pub max_jump_ms: i64,
    /// This many consecutive skipped-but-self-consistent readings trigger a
    /// desync re-sync (missed reset, timer edited, etc.).
    pub desync_confirmations: usize,
    /// A desync that re-syncs onto a timer BELOW this is a real
    /// reset-and-restart (Reset+Started). At or above it, the readings are
    /// mid-run values — stream time slipped (CDN rewind, dropout) — so the
    /// tracker re-anchors silently and the same run continues.
    pub desync_restart_max_ms: i64,
    /// Readings kept for the smoothed "current time" estimate.
    pub smoothing_window: usize,
    /// A frozen timer below this is a reset, not a finish. Guards against
    /// e.g. LiveSplit's negative pre-start offset ("-5.00") being OCR'd as a
    /// constant "5.00" right after an early reset. Set to a bit under the
    /// fastest plausible completed run.
    pub min_final_ms: i64,
    /// LiveSplit's pre-start offset as it reads once the sign is lost
    /// ("-5.00" -> 5000). While a run is well past it, a reading of exactly
    /// this value is the reset screen. 0 disables.
    pub prestart_offset_ms: i64,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            start_epsilon_ms: 500,
            start_confirmations: 3,
            min_advance_ms: 100,
            advance_slack_ms: 2000,
            stall_tolerance_ms: 150,
            stall_confirmations: 5,
            reset_epsilon_ms: 1500,
            reset_confirmations: 2,
            illegible_reset_count: 180,
            max_jump_ms: 5000,
            desync_confirmations: 3,
            desync_restart_max_ms: 90_000,
            smoothing_window: 5,
            min_final_ms: 60_000,
            prestart_offset_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obs {
    /// A parsed timer reading, in milliseconds.
    Time(i64),
    /// OCR produced nothing parseable for this frame.
    Illegible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    Zeroed,
    Disappeared,
    Desync,
    /// Timer froze below min_final_ms — too short to be a real finish.
    TooShort,
}

impl ResetReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResetReason::Zeroed => "zeroed",
            ResetReason::Disappeared => "disappeared",
            ResetReason::Desync => "desync",
            ResetReason::TooShort => "tooshort",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A run is underway; `timer_ms` is the current timer value, so the real
    /// start instant is `now - timer_ms` (also correct when we join mid-run).
    Started {
        timer_ms: i64,
    },
    Finished {
        final_ms: i64,
    },
    Reset {
        last_ms: i64,
        reason: ResetReason,
    },
    /// Stream time slipped (CDN rewind, dropout) and the tracker re-anchored
    /// onto the same run's new apparent timeline. Informational — no run
    /// starts or ends.
    Resynced {
        from_ms: i64,
        to_ms: i64,
    },
}

enum Phase {
    Idle {
        /// Consecutive advancing (wall_ms, timer_ms) candidates.
        chain: Vec<(i64, i64)>,
    },
    Running(Box<Run>),
}

struct Run {
    last_good_at: i64,
    last_good_ms: i64,
    /// Last accepted value; repeats of it feed finish detection.
    streak_value: i64,
    streak_repeats: usize,
    zeroish: usize,
    illegible: usize,
    /// Rejected readings; a self-consistent row of them means WE are the ones
    /// who are wrong (missed reset) and we re-sync.
    suspects: Vec<(i64, i64)>,
    smoother: Smoother,
    /// The previous raw reading, accepted or not: the pre-start offset is
    /// recognised by being frozen, a run at 0:05 by moving on to 0:06.
    last_raw: i64,
}

impl Run {
    fn seed(t: i64, v: i64, cfg: &TrackerConfig) -> Self {
        let mut smoother = Smoother::new(cfg.smoothing_window);
        smoother.push(t, v);
        Self {
            last_good_at: t,
            last_good_ms: v,
            streak_value: v,
            streak_repeats: 0,
            zeroish: 0,
            illegible: 0,
            suspects: Vec::new(),
            smoother,
            last_raw: v,
        }
    }
}

pub struct Tracker {
    cfg: TrackerConfig,
    phase: Phase,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            cfg,
            phase: Phase::Idle { chain: Vec::new() },
        }
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Idle { .. } => "IDLE",
            Phase::Running(_) => "RUNNING",
        }
    }

    /// Best current estimate of the on-screen timer, projected to `now_ms`.
    /// NOTE: this keeps projecting through unreadable stretches by design —
    /// pair it with `accepted_age_ms` to tell a live value from a stale one.
    pub fn smoothed_now(&self, now_ms: i64) -> Option<i64> {
        match &self.phase {
            Phase::Running(run) => run.smoother.current(now_ms),
            Phase::Idle { .. } => None,
        }
    }

    /// How long ago the last reading was actually accepted (ms). Large values
    /// mean `smoothed_now` is a projection, not an observation.
    pub fn accepted_age_ms(&self, now_ms: i64) -> Option<i64> {
        match &self.phase {
            Phase::Running(run) => Some(now_ms - run.last_good_at),
            Phase::Idle { .. } => None,
        }
    }

    pub fn observe(&mut self, t: i64, obs: Obs) -> Vec<Event> {
        let mut events = Vec::new();
        let phase = std::mem::replace(&mut self.phase, Phase::Idle { chain: Vec::new() });
        self.phase = match phase {
            Phase::Idle { chain } => self.observe_idle(t, obs, chain, &mut events),
            Phase::Running(run) => self.observe_running(t, obs, run, &mut events),
        };
        events
    }

    fn observe_idle(
        &self,
        t: i64,
        obs: Obs,
        mut chain: Vec<(i64, i64)>,
        events: &mut Vec<Event>,
    ) -> Phase {
        let cfg = &self.cfg;
        match obs {
            Obs::Illegible => chain.clear(),
            Obs::Time(v) if v <= cfg.start_epsilon_ms => chain.clear(),
            Obs::Time(v) => {
                let extends = match chain.last() {
                    None => true,
                    Some(&(lt, lv)) => {
                        let delta = v - lv;
                        let elapsed = t - lt;
                        delta >= cfg.min_advance_ms
                            && (delta - elapsed).abs() <= cfg.advance_slack_ms
                    }
                };
                if !extends {
                    chain.clear();
                }
                chain.push((t, v));
                if chain.len() >= cfg.start_confirmations {
                    events.push(Event::Started { timer_ms: v });
                    return Phase::Running(Box::new(Run::seed(t, v, cfg)));
                }
            }
        }
        Phase::Idle { chain }
    }

    fn observe_running(
        &self,
        t: i64,
        obs: Obs,
        mut run: Box<Run>,
        events: &mut Vec<Event>,
    ) -> Phase {
        let cfg = &self.cfg;
        let v = match obs {
            Obs::Illegible => {
                run.illegible += 1;
                if run.illegible >= cfg.illegible_reset_count {
                    events.push(Event::Reset {
                        last_ms: run.last_good_ms,
                        reason: ResetReason::Disappeared,
                    });
                    return Phase::Idle { chain: Vec::new() };
                }
                return Phase::Running(run);
            }
            Obs::Time(v) => v,
        };
        run.illegible = 0;
        let last_raw = run.last_raw;
        run.last_raw = v;

        // 1. Same value as last accepted reading: the timer is not advancing.
        //    Checked before the expected-window test, because a frozen timer
        //    drifts ever further from the wall-clock expectation.
        if (v - run.streak_value).abs() <= cfg.stall_tolerance_ms {
            run.streak_repeats += 1;
            run.zeroish = 0;
            run.suspects.clear();
            if run.streak_repeats >= cfg.stall_confirmations {
                if run.streak_value < cfg.min_final_ms {
                    events.push(Event::Reset {
                        last_ms: run.streak_value,
                        reason: ResetReason::TooShort,
                    });
                } else {
                    events.push(Event::Finished {
                        final_ms: run.streak_value,
                    });
                }
                return Phase::Idle { chain: Vec::new() };
            }
            return Phase::Running(run);
        }

        // 2. Advancing consistently with elapsed wall time: a good reading.
        let expected = run.last_good_ms + (t - run.last_good_at);
        let monotonic = v + cfg.stall_tolerance_ms >= run.last_good_ms;
        if monotonic && (v - expected).abs() <= cfg.max_jump_ms {
            run.last_good_at = t;
            run.last_good_ms = v;
            run.streak_value = v;
            run.streak_repeats = 0;
            run.zeroish = 0;
            run.suspects.clear();
            run.smoother.push(t, v);
            return Phase::Running(run);
        }

        // 3. Timer back at ~zero: the runner reset. LiveSplit's pre-start
        //    offset ("-5.00", read as a bare 5.00) counts too when the run
        //    was well past it: it is the reset screen, not a timer value.
        let prestart = cfg.prestart_offset_ms > 0
            && (v - cfg.prestart_offset_ms).abs() <= cfg.stall_tolerance_ms
            && (v - last_raw).abs() <= cfg.stall_tolerance_ms
            && run.last_good_ms > cfg.prestart_offset_ms + 5_000;
        if v < cfg.reset_epsilon_ms || prestart {
            run.zeroish += 1;
            run.suspects.clear();
            if run.zeroish >= cfg.reset_confirmations {
                events.push(Event::Reset {
                    last_ms: run.last_good_ms,
                    reason: ResetReason::Zeroed,
                });
                return Phase::Idle { chain: Vec::new() };
            }
            return Phase::Running(run);
        }

        // 4. A misread — usually a one-off, skipped. But if several rejected
        //    readings in a row agree with EACH OTHER, our baseline is what's
        //    wrong (missed a reset-and-restart, timer edit, long dropout):
        //    close out the old run and re-sync onto the new one.
        run.zeroish = 0;
        // A reading one confusable glyph away from the expected value (a red
        // "7:22" reading as "1:22" for frames on end) is OCR, not a desync;
        // it must not accumulate as evidence of one.
        if glyph_confusion(expected, v) {
            return Phase::Running(run);
        }
        run.suspects.push((t, v));
        if run.suspects.len() > cfg.desync_confirmations {
            run.suspects.remove(0);
        }
        if run.suspects.len() == cfg.desync_confirmations && consistent(&run.suspects, cfg) {
            let &(st, sv) = run.suspects.last().unwrap();
            // A new run: small values, or a value well below where the run
            // was (a timer never runs backwards by more than a rewind can
            // explain — half the run is a restart during an unreadable gap).
            if sv < cfg.desync_restart_max_ms || sv * 2 < run.last_good_ms {
                // Small values: the runner really did reset and restart.
                events.push(Event::Reset {
                    last_ms: run.last_good_ms,
                    reason: ResetReason::Desync,
                });
                events.push(Event::Started { timer_ms: sv });
            } else {
                // Mid-run values: LiveSplit never jumps to these on its own —
                // the stream's clock slipped. Same run, new anchor.
                events.push(Event::Resynced {
                    from_ms: run.last_good_ms,
                    to_ms: sv,
                });
            }
            return Phase::Running(Box::new(Run::seed(st, sv, cfg)));
        }
        Phase::Running(run)
    }
}

/// True when `v` differs from `expected` (±1s of rounding) in exactly one
/// digit of its M:SS part and that digit pair is a known tesseract mix-up —
/// a single-glyph misread rather than a different value on screen.
fn glyph_confusion(expected: i64, v: i64) -> bool {
    const PAIRS: &[(char, char)] = &[
        ('1', '7'),
        ('7', '1'),
        ('2', '7'),
        ('7', '2'),
        ('1', '4'),
        ('4', '1'),
        ('3', '8'),
        ('8', '3'),
        ('0', '8'),
        ('8', '0'),
        ('6', '8'),
        ('8', '6'),
        ('5', '6'),
        ('6', '5'),
        ('0', '6'),
        ('6', '0'),
        ('4', '9'),
        ('9', '4'),
        ('3', '9'),
        ('9', '3'),
    ];
    // A reading under a minute against a clock past one is far likelier a
    // fast reset-and-restart than a 6→0 or 8→0 glyph error, and the cost of
    // getting that wrong is two runs merged into one: no suppression there.
    if v < 60_000 || expected < 60_000 {
        return false;
    }
    let mmss = |ms: i64| {
        let s = ms.max(0) / 1000;
        format!("{}:{:02}", s / 60, s % 60)
    };
    let b = mmss(v);
    [expected - 1000, expected, expected + 1000]
        .iter()
        .any(|&e| {
            let a = mmss(e);
            if a.len() != b.len() {
                return false;
            }
            let diffs: Vec<(char, char)> =
                a.chars().zip(b.chars()).filter(|(x, y)| x != y).collect();
            diffs.len() == 1 && PAIRS.contains(&diffs[0])
        })
}

fn consistent(readings: &[(i64, i64)], cfg: &TrackerConfig) -> bool {
    readings.windows(2).all(|w| {
        let (t0, v0) = w[0];
        let (t1, v1) = w[1];
        let delta = v1 - v0;
        let elapsed = t1 - t0;
        delta >= cfg.min_advance_ms && (delta - elapsed).abs() <= cfg.advance_slack_ms
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the tracker one reading per simulated second, like the 1 fps
    /// capture pipeline does.
    struct Sim {
        tr: Tracker,
        t: i64,
    }

    impl Sim {
        fn new(cfg: TrackerConfig) -> Self {
            Self {
                tr: Tracker::new(cfg),
                t: 0,
            }
        }

        fn time(&mut self, ms: i64) -> Vec<Event> {
            self.t += 1000;
            self.tr.observe(self.t, Obs::Time(ms))
        }

        fn illegible(&mut self) -> Vec<Event> {
            self.t += 1000;
            self.tr.observe(self.t, Obs::Illegible)
        }

        /// Feed an advancing timer from `from` to `to` inclusive, stepping 1s,
        /// asserting no events fire along the way.
        fn advance_quietly(&mut self, from: i64, to: i64) {
            let mut v = from;
            while v <= to {
                assert_eq!(self.time(v), vec![], "unexpected event at timer {v}");
                v += 1000;
            }
        }

        /// Get a run started and advanced to roughly `up_to`.
        fn start_run(&mut self, up_to: i64) {
            assert_eq!(self.time(0), vec![]);
            assert_eq!(self.time(1000), vec![]);
            assert_eq!(self.time(2000), vec![]);
            assert_eq!(self.time(3000), vec![Event::Started { timer_ms: 3000 }]);
            self.advance_quietly(4000, up_to);
        }
    }

    fn cfg() -> TrackerConfig {
        TrackerConfig::default()
    }

    #[test]
    fn starts_after_consistent_advance() {
        let mut s = Sim::new(cfg());
        assert_eq!(s.time(0), vec![]);
        assert_eq!(s.time(0), vec![]);
        assert_eq!(s.time(1000), vec![]);
        assert_eq!(s.time(2000), vec![]);
        assert_eq!(s.time(3000), vec![Event::Started { timer_ms: 3000 }]);
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn frozen_timer_never_starts_a_run() {
        // e.g. bot launched while a finished time sits frozen on screen.
        let mut s = Sim::new(cfg());
        for _ in 0..20 {
            assert_eq!(s.time(1_234_500), vec![]);
        }
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn inconsistent_jumps_never_start_a_run() {
        let mut s = Sim::new(cfg());
        for v in [5000, 90_000, 12_000, 400_000, 33_000, 700_000] {
            assert_eq!(s.time(v), vec![]);
        }
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn joins_a_run_already_in_progress() {
        let mut s = Sim::new(cfg());
        assert_eq!(s.time(1_500_000), vec![]);
        assert_eq!(s.time(1_501_000), vec![]);
        assert_eq!(
            s.time(1_502_000),
            vec![Event::Started {
                timer_ms: 1_502_000
            }]
        );
    }

    #[test]
    fn finishes_when_timer_freezes() {
        let mut s = Sim::new(cfg());
        s.start_run(90_000);
        // Timer frozen at 90s: first 4 repeats are quiet, 5th declares it.
        for _ in 0..4 {
            assert_eq!(s.time(90_000), vec![]);
        }
        assert_eq!(s.time(90_000), vec![Event::Finished { final_ms: 90_000 }]);
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn finish_detection_survives_illegible_gaps() {
        // Legible frozen frames interleaved with illegible ones (overlay
        // flicker) still add up to a finish.
        let mut s = Sim::new(cfg());
        s.start_run(90_000);
        for i in 0..4 {
            assert_eq!(s.time(90_000), vec![], "repeat {i}");
            assert_eq!(s.illegible(), vec![]);
        }
        assert_eq!(s.time(90_000), vec![Event::Finished { final_ms: 90_000 }]);
    }

    #[test]
    fn pause_and_resume_is_not_a_finish() {
        let mut s = Sim::new(cfg());
        s.start_run(30_000);
        // Four frozen readings — one short of a finish — then it moves again.
        for _ in 0..4 {
            assert_eq!(s.time(30_000), vec![]);
        }
        // 5s of wall time passed while frozen; timer resumes from 31s and
        // stays within the expected window (30s+5s elapsed vs 31s read).
        assert_eq!(s.time(31_000), vec![]);
        s.advance_quietly(32_000, 40_000);
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn short_freeze_is_a_reset_not_a_finish() {
        // Runner dies at ~5s and resets; LiveSplit's "-5.00" pre-start offset
        // OCRs as a constant "5.00" that would otherwise look like a finish.
        let mut s = Sim::new(cfg());
        s.start_run(5000);
        for _ in 0..4 {
            assert_eq!(s.time(5000), vec![]);
        }
        assert_eq!(
            s.time(5000),
            vec![Event::Reset {
                last_ms: 5000,
                reason: ResetReason::TooShort
            }]
        );
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn resets_when_timer_returns_to_zero() {
        let mut s = Sim::new(cfg());
        s.start_run(45_000);
        assert_eq!(s.time(0), vec![]);
        assert_eq!(
            s.time(0),
            vec![Event::Reset {
                last_ms: 45_000,
                reason: ResetReason::Zeroed
            }]
        );
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn single_zero_misread_does_not_reset() {
        let mut s = Sim::new(cfg());
        s.start_run(45_000);
        assert_eq!(s.time(0), vec![]); // one bad frame
        s.advance_quietly(47_000, 55_000); // clock resumes exactly on schedule
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn wild_misreads_are_skipped() {
        let mut s = Sim::new(cfg());
        s.start_run(90_000);
        assert_eq!(s.time(999_000), vec![]); // "99:00" style misread
        s.advance_quietly(92_000, 100_000);
        for _ in 0..4 {
            assert_eq!(s.time(100_000), vec![]);
        }
        assert_eq!(s.time(100_000), vec![Event::Finished { final_ms: 100_000 }]);
    }

    #[test]
    fn disappearance_is_a_dnf() {
        let mut c = cfg();
        c.illegible_reset_count = 6;
        let mut s = Sim::new(c);
        s.start_run(60_000);
        for _ in 0..5 {
            assert_eq!(s.illegible(), vec![]);
        }
        assert_eq!(
            s.illegible(),
            vec![Event::Reset {
                last_ms: 60_000,
                reason: ResetReason::Disappeared
            }]
        );
        assert_eq!(s.tr.phase_name(), "IDLE");
    }

    #[test]
    fn short_ocr_dropout_does_not_dnf() {
        let mut s = Sim::new(cfg());
        s.start_run(60_000);
        for _ in 0..30 {
            assert_eq!(s.illegible(), vec![]);
        }
        // 30s later the timer is legible again, exactly 30s further along.
        assert_eq!(s.time(91_000), vec![]);
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn clock_slip_reanchors_without_phantom_runs() {
        // Stream time slipped (CDN rewind / dropout): readings land on a
        // consistent mid-run timeline. Same run — re-anchor, no Reset/Started.
        let mut s = Sim::new(cfg());
        s.start_run(60_000);
        assert_eq!(s.time(300_000), vec![]);
        assert_eq!(s.time(301_000), vec![]);
        assert_eq!(
            s.time(302_000),
            vec![Event::Resynced {
                from_ms: 60_000,
                to_ms: 302_000
            }]
        );
        assert_eq!(s.tr.phase_name(), "RUNNING");
        // ...and a backward slip (rewind) too.
        assert_eq!(s.time(150_000), vec![]);
        assert_eq!(s.time(151_000), vec![]);
        assert_eq!(
            s.time(152_000),
            vec![Event::Resynced {
                from_ms: 302_000,
                to_ms: 152_000
            }]
        );
        s.advance_quietly(153_000, 160_000);
    }

    #[test]
    fn fast_reset_and_restart_is_caught_via_desync() {
        // Runner resets and immediately restarts: we only catch one near-zero
        // frame, then the new run is already advancing.
        let mut s = Sim::new(cfg());
        s.start_run(60_000);
        assert_eq!(s.time(600), vec![]); // one zeroish frame
        assert_eq!(s.time(1600), vec![]);
        assert_eq!(s.time(2600), vec![]);
        let events = s.time(3600);
        assert_eq!(
            events,
            vec![
                Event::Reset {
                    last_ms: 60_000,
                    reason: ResetReason::Desync
                },
                Event::Started { timer_ms: 3600 },
            ]
        );
    }

    #[test]
    fn red_seven_read_as_one_is_not_a_desync() {
        // Past 7:00 with a red (behind-pace) timer the "7" reads as "1" for
        // frames on end: 7:22 → 1:22, 7:23 → 1:23 ... Those are one glyph
        // off the expected value and must not close the run.
        let mut s = Sim::new(cfg());
        s.start_run(440_000); // 7:20
        for v in [81_000, 82_000, 83_000, 84_000, 85_000, 86_000] {
            assert_eq!(s.time(v), vec![], "1:{} must be ignored", v / 1000 % 60);
        }
        assert_eq!(s.tr.phase_name(), "RUNNING");
        // Reading recovers; the same run continues without any event.
        s.advance_quietly(447_000, 460_000);
        // A genuine reset-and-restart is still caught: many glyphs differ.
        assert_eq!(s.time(2_000), vec![]);
        assert_eq!(s.time(3_000), vec![]);
        let ev = s.time(4_000);
        assert!(
            matches!(
                ev.first(),
                Some(Event::Reset {
                    reason: ResetReason::Desync,
                    ..
                })
            ),
            "{ev:?}"
        );
    }

    #[test]
    fn glyph_confusion_pairs() {
        assert!(glyph_confusion(442_000, 82_000)); // 7:22 vs 1:22
        assert!(glyph_confusion(442_600, 82_000)); // rounding slack
        assert!(glyph_confusion(82_000, 442_000)); // and the reverse
        assert!(!glyph_confusion(442_000, 4_000)); // 7:22 vs 0:04
        assert!(!glyph_confusion(442_000, 122_000)); // 7:22 vs 2:02: two glyphs
        assert!(!glyph_confusion(442_000, 262_000)); // 7:22 vs 4:22: not a known pair
                                                     // Under a minute is never a glyph confusion: 6:03 vs 0:03 is a restart.
        assert!(!glyph_confusion(363_000, 3_000));
        assert!(!glyph_confusion(483_000, 3_000)); // 8:03 vs 0:03
    }

    #[test]
    fn restart_after_a_long_unreadable_gap_is_a_reset_not_a_resync() {
        // Run at 7:00, feed dies for 2 minutes, runner resets and restarts;
        // readings resume at 1:40 — under half the run, so a new run, even
        // though 100 s is past desync_restart_max_ms.
        let mut s = Sim::new(cfg());
        s.start_run(420_000);
        for _ in 0..120 {
            assert_eq!(s.illegible(), vec![]);
        }
        assert_eq!(s.time(100_000), vec![]);
        assert_eq!(s.time(101_000), vec![]);
        let ev = s.time(102_000);
        assert!(
            matches!(
                ev.first(),
                Some(Event::Reset {
                    reason: ResetReason::Desync,
                    ..
                })
            ),
            "{ev:?}"
        );
        assert!(
            matches!(ev.get(1), Some(Event::Started { timer_ms: 102_000 })),
            "{ev:?}"
        );
        // A rewind of a minute on a 7-minute run is still a clock slip.
        let mut s = Sim::new(cfg());
        s.start_run(420_000);
        assert_eq!(s.time(360_000), vec![]);
        assert_eq!(s.time(361_000), vec![]);
        let ev = s.time(362_000);
        assert!(matches!(ev.first(), Some(Event::Resynced { .. })), "{ev:?}");
    }

    #[test]
    fn prestart_offset_reads_as_a_reset_mid_run() {
        // LiveSplit shows -5.00 after a reset; OCR reads "5.00". Once it has
        // been seen frozen (the first 5.00 could still be a run at 0:05),
        // two such frames while the run was at 3:00 mean the runner reset.
        let mut s = Sim::new(cfg());
        s.start_run(180_000);
        assert_eq!(s.time(5_000), vec![]);
        assert_eq!(s.time(5_000), vec![]);
        let ev = s.time(5_000);
        assert!(
            matches!(
                ev.first(),
                Some(Event::Reset {
                    reason: ResetReason::Zeroed,
                    last_ms: 180_000
                })
            ),
            "{ev:?}"
        );
        // But 5.00 in the first seconds of a run is just the timer at 5s.
        let mut s = Sim::new(cfg());
        s.start_run(4_000);
        assert_eq!(s.time(5_000), vec![]);
        assert_eq!(s.time(6_000), vec![]);
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn six_minute_fast_restart_is_still_a_reset() {
        // Runner dies at 6:00 and restarts so fast that the first reading of
        // the new run is already 0:03 — a 6/0 glyph pair, but under a minute.
        let mut s = Sim::new(cfg());
        s.start_run(360_000);
        assert_eq!(s.time(3_000), vec![]);
        assert_eq!(s.time(4_000), vec![]);
        let ev = s.time(5_000);
        assert!(
            matches!(
                ev.first(),
                Some(Event::Reset {
                    reason: ResetReason::Desync,
                    ..
                })
            ),
            "{ev:?}"
        );
        assert!(
            matches!(ev.get(1), Some(Event::Started { timer_ms: 5_000 })),
            "{ev:?}"
        );
    }

    #[test]
    fn random_noise_does_not_desync() {
        let mut s = Sim::new(cfg());
        s.start_run(30_000);
        // Mutually inconsistent garbage readings.
        for v in [500_000, 120_000, 750_000, 90_000] {
            assert_eq!(s.time(v), vec![]);
        }
        // Real timer still where the wall clock says it should be.
        s.advance_quietly(35_000, 45_000);
        assert_eq!(s.tr.phase_name(), "RUNNING");
    }

    #[test]
    fn full_session_two_runs() {
        let mut s = Sim::new(cfg());
        // Attempt 1: reset at 20s.
        s.start_run(20_000);
        assert_eq!(s.time(100), vec![]);
        assert_eq!(
            s.time(100),
            vec![Event::Reset {
                last_ms: 20_000,
                reason: ResetReason::Zeroed
            }]
        );
        // Attempt 2: finishes at 75s.
        assert_eq!(s.time(1000), vec![]);
        assert_eq!(s.time(2000), vec![]);
        assert_eq!(s.time(3000), vec![Event::Started { timer_ms: 3000 }]);
        s.advance_quietly(4000, 75_000);
        for _ in 0..4 {
            assert_eq!(s.time(75_000), vec![]);
        }
        assert_eq!(s.time(75_000), vec![Event::Finished { final_ms: 75_000 }]);
    }

    #[test]
    fn accepted_age_exposes_staleness() {
        let mut s = Sim::new(cfg());
        s.start_run(10_000);
        assert_eq!(s.tr.accepted_age_ms(s.t), Some(0));
        // Nothing readable for 30s: smoothed keeps projecting, age says so.
        for _ in 0..30 {
            s.illegible();
        }
        assert_eq!(s.tr.accepted_age_ms(s.t), Some(30_000));
        assert!(s.tr.smoothed_now(s.t).unwrap() >= 40_000);
        // Back to idle: no age.
        assert_eq!(s.time(0), vec![]);
        s.time(0);
        assert_eq!(s.tr.accepted_age_ms(s.t), None);
    }

    #[test]
    fn smoothed_time_tracks_running_timer() {
        let mut s = Sim::new(cfg());
        s.start_run(10_000);
        let now = s.t;
        let smoothed = s.tr.smoothed_now(now).unwrap();
        assert!((smoothed - 10_000).abs() <= 100, "smoothed={smoothed}");
        assert_eq!(s.tr.smoothed_now(now + 5000).unwrap(), smoothed + 5000);
    }
}
