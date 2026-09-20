//! The layout lock as a state machine.
//!
//! The frame loop reads the timer off one of several configured layouts,
//! and which one — and whether any — is a state that eleven variables used
//! to carry between them: `layout_locked`, `ever_locked`, `board_locked`,
//! the board threshold and grant time, the dark-frame count, the quality
//! window, the clipped streak, the board probe's hit count and cadence.
//! Its defects were interactions between two of them: timer candidates
//! re-granting a lock the board had granted, the grant's geometry pass
//! under a board lock, the quality judge unlocking a board lock over a
//! timer it was never meant to read. This is those variables as one value
//! with explicit transitions, in the shape `state.rs` gives the run: pure,
//! and tested on its own.
//!
//! Three states:
//!
//! - **Probing**: no layout holds the lock. Timer candidates compete (a
//!   consistent reading on five looks wins; ten to switch scenes once a
//!   lock has ever held), and every twenty frames the pane is read as a
//!   board against every layout.
//! - **Timer**: a layout won by its timer. The timer's reads judge it: dark
//!   for `dark_frames` frames, under 40% parsed over a 60-frame window, or
//!   ten clipped reads in a row, and the lock is let go. The board probe
//!   still runs, at a third of its cadence — the default crop lies over the
//!   race board's time cells and locks on them as a timer first.
//! - **Board**: a layout won by its pane reading as a board tracked by its
//!   rows, twice running. The timer's reads judge nothing here — on that
//!   board the timer is not what is tracked, and may not be readable where
//!   it sits — and timer candidates do not compete. The lock is let go
//!   when no marathon is in force `board_hold_ms` after the grant: the event
//!   ended, or the pane stopped reading as one before a tracker was ever
//!   taken up.
//!
//! A single configured layout is locked from the start and never let go:
//! there is nothing to probe for.

/// Which layout holds the lock, and by what right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Probing,
    Timer {
        layout: usize,
        off: (i32, i32),
    },
    Board {
        layout: usize,
        threshold: u8,
        since_ms: i64,
    },
}

/// Why a lock was let go, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlock {
    /// The timer read nothing for this many frames.
    Dark { frames: u32 },
    /// The timer parsed on too few of the judged frames, or read clipped
    /// this many times running.
    Poor {
        parsed: u32,
        frames: u32,
        clipped: u32,
    },
    /// A board-granted lock with no marathon in force after the hold.
    NoMarathon,
}

/// The thresholds. The defaults are the frame loop's; `dark_frames` comes
/// from `layout_search.dark_frames_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockCfg {
    pub dark_frames: u32,
    pub quality_window: u32,
    pub quality_min_pct: u32,
    pub clipped_limit: u32,
    pub board_probe_unlocked: u32,
    pub board_probe_locked: u32,
    pub board_hits: u32,
    pub board_hold_ms: i64,
    pub streak_same: u32,
    pub streak_switch: u32,
}

impl LockCfg {
    pub fn with_dark_frames(dark_frames: u32) -> Self {
        LockCfg {
            dark_frames: dark_frames.max(1),
            quality_window: 60,
            quality_min_pct: 40,
            clipped_limit: 10,
            board_probe_unlocked: 20,
            board_probe_locked: 60,
            board_hits: 2,
            board_hold_ms: 300_000,
            streak_same: 5,
            streak_switch: 10,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LockState {
    cfg: LockCfg,
    lock: Lock,
    /// Whether any lock has ever held: afterwards the last active layout is
    /// favoured and a switch needs the longer streak.
    ever_locked: bool,
    /// One configured layout: locked from the start, never let go.
    single: bool,
    dark: u32,
    quality_frames: u32,
    quality_parsed: u32,
    clipped: u32,
    board_hit: Option<(usize, u32)>,
    board_probe_frames: u32,
}

impl LockState {
    pub fn new(cfg: LockCfg, layouts: usize) -> Self {
        let single = layouts <= 1;
        LockState {
            cfg,
            lock: if single {
                Lock::Timer {
                    layout: 0,
                    off: (0, 0),
                }
            } else {
                Lock::Probing
            },
            ever_locked: single,
            single,
            dark: 0,
            quality_frames: 0,
            quality_parsed: 0,
            clipped: 0,
            board_hit: None,
            board_probe_frames: 0,
        }
    }

    pub fn lock(&self) -> Lock {
        self.lock
    }

    pub fn locked(&self) -> bool {
        !matches!(self.lock, Lock::Probing)
    }

    pub fn ever_locked(&self) -> bool {
        self.ever_locked
    }

    /// The board-granted lock's layout and threshold, when that is the lock.
    pub fn board(&self) -> Option<(usize, u8)> {
        match self.lock {
            Lock::Board {
                layout, threshold, ..
            } => Some((layout, threshold)),
            _ => None,
        }
    }

    pub fn is_board(&self) -> bool {
        self.board().is_some()
    }

    /// Whether timer candidates may win the lock: never while the board
    /// holds it. This board's timer reads whole seconds where it reads at
    /// all, and a streak of those re-anchored the layout every minute.
    pub fn timer_candidates_compete(&self) -> bool {
        !self.is_board()
    }

    /// How many consistent looks a timer candidate needs: five to find the
    /// scene, or the same scene again; ten to switch to another layout once
    /// a lock has ever held, so an overlapping rectangle cannot win merely
    /// by being tried first.
    pub fn need(&self, layout: usize, active: usize) -> u32 {
        if !self.ever_locked || layout == active {
            self.cfg.streak_same
        } else {
            self.cfg.streak_switch
        }
    }

    /// One frame passed in the probing branch: is the board probe due? Every
    /// `board_probe_unlocked` frames while probing, every
    /// `board_probe_locked` while a timer holds the lock, never while the
    /// board does — and never where nothing is tracked by its rows.
    pub fn board_probe_due(&mut self, has_board_alias: bool) -> bool {
        self.board_probe_frames += 1;
        if !has_board_alias || self.is_board() {
            return false;
        }
        let every = if self.locked() {
            self.cfg.board_probe_locked
        } else {
            self.cfg.board_probe_unlocked
        };
        if self.board_probe_frames >= every {
            self.board_probe_frames = 0;
            true
        } else {
            false
        }
    }

    /// The board probe's reading: the layout whose pane read as a board and
    /// the threshold it read at, or nothing. The same layout `board_hits`
    /// times running grants it; a different layout starts the count over,
    /// and a miss clears it.
    pub fn board_probe(&mut self, found: Option<(usize, u8)>) -> Option<(usize, u8)> {
        self.board_hit = match (self.board_hit, found) {
            (Some((l, n)), Some((li, _))) if l == li => Some((l, n + 1)),
            (_, Some((li, _))) => Some((li, 1)),
            (_, None) => None,
        };
        match (self.board_hit, found) {
            (Some((_, n)), Some(f)) if n >= self.cfg.board_hits => {
                self.board_hit = None;
                Some(f)
            }
            _ => None,
        }
    }

    /// The board grants the lock to `layout`, read at `threshold`, at `t`.
    pub fn grant_board(&mut self, layout: usize, threshold: u8, t: i64) {
        self.lock = Lock::Board {
            layout,
            threshold,
            since_ms: t,
        };
        self.ever_locked = true;
        self.reset_judges();
    }

    /// A timer candidate won: `layout` at `off`. Not while the board holds
    /// the lock — the winner path is shared, and a board grant has set the
    /// lock already.
    pub fn grant_timer(&mut self, layout: usize, off: (i32, i32)) {
        if self.is_board() {
            return;
        }
        self.lock = Lock::Timer { layout, off };
        self.ever_locked = true;
        self.reset_judges();
    }

    /// The winner path begins: the judges start over on the new position.
    pub fn reset_judges(&mut self) {
        self.quality_frames = 0;
        self.quality_parsed = 0;
        self.clipped = 0;
        self.dark = 0;
    }

    /// A locked timer read came back clipped against the crop edge, or not:
    /// a run of clipped reads is a wrong position even when it parses.
    pub fn timer_clipped(&mut self, clipped: bool) {
        if clipped {
            self.clipped += 1;
        } else {
            self.clipped = 0;
        }
    }

    /// One frame's timer reading against the lock. Only a timer-granted
    /// lock is judged by it, and only where there is something else to
    /// probe for; a static frame is not judged (it repeats the last read).
    pub fn timer_read(&mut self, parsed: bool, frame_static: bool) -> Option<Unlock> {
        if self.single || !self.locked() || self.is_board() {
            return None;
        }
        if parsed {
            self.dark = 0;
        } else {
            self.dark += 1;
            if self.dark >= self.cfg.dark_frames {
                let frames = self.dark;
                self.unlock();
                return Some(Unlock::Dark { frames });
            }
        }
        if frame_static {
            return None;
        }
        self.quality_frames += 1;
        if parsed {
            self.quality_parsed += 1;
        }
        let poor = self.quality_frames >= self.cfg.quality_window
            && self.quality_parsed * 100 / self.quality_frames < self.cfg.quality_min_pct;
        if poor || self.clipped >= self.cfg.clipped_limit {
            let u = Unlock::Poor {
                parsed: self.quality_parsed,
                frames: self.quality_frames,
                clipped: self.clipped,
            };
            self.unlock();
            return Some(u);
        }
        if self.quality_frames >= self.cfg.quality_window {
            self.quality_frames = 0;
            self.quality_parsed = 0;
        }
        None
    }

    /// One frame passed: a board-granted lock with no marathon in force
    /// `board_hold_ms` after the grant is let go.
    pub fn tick(&mut self, t: i64, marathon_active: bool) -> Option<Unlock> {
        match self.lock {
            Lock::Board { since_ms, .. }
                if !marathon_active && t - since_ms > self.cfg.board_hold_ms =>
            {
                self.unlock();
                Some(Unlock::NoMarathon)
            }
            _ => None,
        }
    }

    fn unlock(&mut self) {
        self.lock = Lock::Probing;
        self.reset_judges();
        self.board_hit = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(layouts: usize) -> LockState {
        LockState::new(LockCfg::with_dark_frames(30), layouts)
    }

    #[test]
    fn a_single_layout_is_locked_from_the_start_and_never_let_go() {
        let mut s = st(1);
        assert!(s.locked());
        assert!(s.ever_locked());
        for _ in 0..500 {
            assert_eq!(s.timer_read(false, false), None);
        }
        assert!(s.locked());
    }

    #[test]
    fn the_board_probe_grants_on_the_same_layout_twice_running() {
        let mut s = st(6);
        assert_eq!(s.board_probe(Some((5, 100))), None);
        // A different layout starts the count over.
        assert_eq!(s.board_probe(Some((2, 100))), None);
        assert_eq!(s.board_probe(Some((5, 100))), None);
        // A miss clears it.
        assert_eq!(s.board_probe(None), None);
        assert_eq!(s.board_probe(Some((5, 100))), None);
        assert_eq!(s.board_probe(Some((5, 100))), Some((5, 100)));
        s.grant_board(5, 100, 1_000);
        assert_eq!(s.board(), Some((5, 100)));
        assert!(!s.timer_candidates_compete());
    }

    #[test]
    fn the_probe_runs_every_twenty_frames_probing_and_every_sixty_locked() {
        let mut s = st(6);
        let due = |s: &mut LockState, n: u32| (0..n).filter(|_| s.board_probe_due(true)).count();
        assert_eq!(due(&mut s, 60), 3);
        s.grant_timer(0, (0, 0));
        assert_eq!(due(&mut s, 60), 1);
        // Never while the board holds the lock, and never without an entry
        // tracked by its rows.
        s.grant_board(5, 100, 0);
        assert_eq!(due(&mut s, 200), 0);
        let mut t = st(6);
        assert_eq!((0..200).filter(|_| t.board_probe_due(false)).count(), 0);
    }

    #[test]
    fn a_board_lock_is_not_judged_by_the_timer_and_lets_go_without_a_marathon() {
        let mut s = st(6);
        s.grant_board(5, 100, 1_000);
        for _ in 0..200 {
            s.timer_clipped(true);
            assert_eq!(s.timer_read(false, false), None);
        }
        assert!(s.is_board());
        // A timer candidate winning the shared grant path changes nothing.
        s.grant_timer(0, (11, 8));
        assert!(s.is_board());
        // In force: held past the hold. Not in force: let go after it.
        assert_eq!(s.tick(1_000 + 400_000, true), None);
        assert_eq!(s.tick(1_000 + 200_000, false), None);
        assert_eq!(s.tick(1_000 + 300_001, false), Some(Unlock::NoMarathon));
        assert!(!s.locked());
        assert_eq!(s.board(), None);
    }

    #[test]
    fn a_timer_lock_is_let_go_dark_poor_or_clipped() {
        let mut s = st(6);
        s.grant_timer(1, (0, 0));
        for _ in 0..29 {
            assert_eq!(s.timer_read(false, false), None);
        }
        assert_eq!(
            s.timer_read(false, false),
            Some(Unlock::Dark { frames: 30 })
        );
        assert!(!s.locked());

        let mut s = st(6);
        s.grant_timer(1, (0, 0));
        // Under 40% parsed over sixty judged frames: one parsed in three,
        // the dark count never reaching thirty.
        let mut got = None;
        for i in 0..60 {
            got = s.timer_read(i % 3 == 0, false);
        }
        assert!(
            matches!(
                got,
                Some(Unlock::Poor {
                    parsed: 20,
                    frames: 60,
                    ..
                })
            ),
            "{got:?}"
        );

        let mut s = st(6);
        s.grant_timer(1, (0, 0));
        for _ in 0..9 {
            s.timer_clipped(true);
            assert_eq!(s.timer_read(true, false), None);
        }
        s.timer_clipped(true);
        assert!(matches!(
            s.timer_read(true, false),
            Some(Unlock::Poor { clipped: 10, .. })
        ));

        // A healthy lock: the quality window resets and nothing is let go.
        let mut s = st(6);
        s.grant_timer(1, (0, 0));
        for _ in 0..300 {
            assert_eq!(s.timer_read(true, false), None);
        }
        assert!(s.locked());
    }

    #[test]
    fn a_switch_needs_the_longer_streak_once_a_lock_has_held() {
        let mut s = st(6);
        assert_eq!(s.need(3, 0), 5, "nothing has ever locked: five anywhere");
        s.grant_timer(0, (0, 0));
        assert_eq!(s.need(0, 0), 5);
        assert_eq!(s.need(3, 0), 10);
    }
}
