//! Marathon boards: ten different games back to back, tracked by what the
//! board says is finished rather than by the timer.
//!
//! The streamer runs two kinds of day. On a Ninja Gaiden day the pane times
//! one game, the timer resets, and every attempt is a run — the state machine
//! in `state.rs` reads that. On an "Arcathlon" day he plays ten NES games in
//! a row, one LiveSplit split row per game, and the pane's big timer is the
//! marathon TOTAL: it pauses between games, never resets, and finishing a
//! game is not a thing that happens to it. Driving the run state machine from
//! it would record one four-hour run, or ten runs whose times are totals.
//!
//! So this module tracks the ROWS. A marathon has no resets — he plays each
//! game to the end — so every row completes exactly once, and when it does
//! the board itself prints the authoritative segment and cumulative time.
//! One completed row is one finished run of that game, filed under the row's
//! own name (`Astyanax`, `SMB3 (Warpless)`), so his Astyanax times accumulate
//! across events in the same `runs.game` every stats, report and chat query
//! already groups by.
//!
//! What makes it harder than "a row gained a time":
//!
//! - **A numbered event prints comparison times from the first frame.**
//!   "Arcathlon #6" lists its ten games alphabetically with the times to
//!   beat, so a cumulative on the board does not mean the game is done. What
//!   means it is done is that the cumulative CHANGED from the one the row
//!   showed when the board came into view — by five seconds on one measured
//!   case (Jurassic Park, comparison 1:27:37, result 1:27:42), so the
//!   comparison is exact, never within a tolerance.
//! - **A randomized event reveals each game as it is drawn**, "???" in the
//!   slots still to come and "-" in their time columns. There the baseline is
//!   the absence of a time, and a "???" row gaining a name is a game drawn.
//! - **OCR damages one row per pass or so.** A cumulative is taken only once
//!   two passes a good minute apart agree on it. The board is static text,
//!   and static text misread once is misread the same way again ten seconds
//!   later off nearly the same pixels — a completed 23:19 read "23:10" on two
//!   consecutive ten-second passes of one broadcast — so agreement has to be
//!   spread out to be worth anything. And because a completion is permanent,
//!   a row that goes back to the value it had never had a completion at all.
//! - **A row that goes unread shifts every row below it** onto somebody
//!   else's game, and the names are what catch that. Rows are placed by
//!   position and the names have to bear the placing out; a pass with fewer
//!   rows than the board has is NOT automatically a shift, because the pane's
//!   bottom edge picks up the line of text under it now and then and leaves
//!   the board one row longer than it really is.
//! - **A row's name is read several ways**, and the pane's edge puts a stray
//!   letter in front of it more often than not, so the spellings are grouped
//!   before they are counted and the run is filed under the one they agree on.
//! - **A completed row keeps its time for the rest of the event**, so a
//!   completion has to be recorded once and only once. Every slot is recorded
//!   at most once here, and `seed` re-arms that across a restart from what is
//!   already in the database.
//!
//! Nothing in this module talks to the database or the clock: `observe` takes
//! a board and returns the completions to record, which is what makes it
//! testable against boards copied out of a real broadcast.

use std::collections::HashMap;

use crate::board::{game_matches, Board, BoardRow};
use crate::config::{Config, GameAlias, GameMode};
use crate::timeparse::{parse_time, time_shaped};

/// Passes that must agree before a cumulative time is believed. The board is
/// static text re-read once a minute, so a real value repeats and a digit
/// slip does not.
const AGREE: u32 = 2;

/// How far apart those readings must lie. The pane pass runs every ten
/// seconds until the board's geometry settles and every minute afterwards,
/// and static text misread once is misread the same way again ten seconds
/// later off nearly the same pixels — measured: a completed row's 23:19 read
/// "23:10" on two consecutive ten-second passes and on no other pass of the
/// broadcast. A minute apart the frame is a different frame.
const AGREE_SPREAD_MS: i64 = 45_000;

/// How far a row's own segment column may sit from the difference between
/// its cumulative and the previous game's before the reading is held back
/// for another pass. The board prints whole seconds.
const SEGMENT_SLACK_MS: i64 = 1000;

/// Passes to wait for the segment column to agree with that difference
/// before recording the difference instead and saying so.
const SEGMENT_PATIENCE: u32 = 4;

/// How far a completion's cumulative may exceed the marathon total that was
/// last read. At the moment a game ends the two are the same value; a
/// comparison time still to be run is minutes ahead of the total, which is
/// what this rejects.
const AHEAD_OF_TOTAL_MS: i64 = 60_000;

/// How far behind the marathon total a cumulative may be and still be a game
/// that has only just finished. Three minutes: the board is read once a
/// minute, and the total is frozen through the pause between games anyway.
const JUST_NOW_MS: i64 = 180_000;

/// A game the board says is finished, ready to be recorded as a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    /// Which row of the board (0-based, top down).
    pub slot: usize,
    /// The row's own name, as the board prints it.
    pub game: String,
    /// The event: the `[[games]]` entry's name.
    pub category: String,
    /// The row's segment time — this game's run.
    pub segment_ms: i64,
    /// The row's cumulative time: the marathon total when it ended, and the
    /// completion's identity for reconciling against the database.
    pub cumulative_ms: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    /// Set when the segment column never agreed with the difference between
    /// this row's cumulative and the previous game's, and the difference was
    /// recorded instead.
    pub segment_derived: bool,
}

/// What the board showed for one row on one pass.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cells {
    segment_ms: Option<i64>,
    cumulative_ms: Option<i64>,
}

/// The cumulative a row showed when the board came into view.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum Baseline {
    /// Not settled yet; no completion can be read off this row.
    #[default]
    Unknown,
    /// The row had no time of its own: a randomized event's undrawn or
    /// unfinished game.
    Empty,
    /// A numbered event's comparison time.
    Was(i64),
}

/// How often a value was read off a row, and over what stretch of the
/// broadcast.
#[derive(Debug, Clone, Copy, Default)]
struct Vote {
    count: u32,
    first_ms: i64,
    last_ms: i64,
}

impl Vote {
    fn spread_ms(&self) -> i64 {
        self.last_ms - self.first_ms
    }
}

#[derive(Debug, Default, Clone)]
struct Slot {
    /// Every spelling of the row's name that was legible, by how often it was
    /// read. The board prints one name and tesseract makes several of it.
    names: HashMap<String, u32>,
    baseline: Baseline,
    /// Votes for what the baseline is, until one of them reaches `AGREE`.
    /// `None` stands for "no time in the row's columns".
    baseline_votes: HashMap<Option<i64>, u32>,
    /// Readings of a cumulative that differs from the baseline: a completion
    /// in the making.
    cumulative_votes: HashMap<i64, Vote>,
    /// Votes for the segment column, per cumulative it was read beside.
    segment_votes: HashMap<(i64, i64), u32>,
    /// Passes spent with a confirmed cumulative whose segment column will not
    /// agree with the previous game's.
    segment_waits: u32,
    /// The cumulative this row was recorded with, once it has been.
    recorded: Option<i64>,
}

impl Slot {
    /// The name to file this row's run under.
    ///
    /// The spellings are grouped before they are counted. The pane's left
    /// border and the highlight bar's edge come through as a letter or two in
    /// front of the name, and they do it often: on one broadcast "a Batman:
    /// ROTJ" outnumbered "Batman: ROTJ" 62 readings to 49, with "s", "e",
    /// "es", "4" and "sa" in front of the rest. So a spelling that is another
    /// with a token or two ahead of it is the same name, and its readings
    /// count towards the shorter one — which "SMB3 (Warpless)" and
    /// "(Warpless)" are not, four characters apart being a lost word rather
    /// than a smudge.
    ///
    /// Then the most read wins; on a tie the longer spelling, so a truncated
    /// reading never beats the whole name, and then alphabetically so the
    /// choice does not depend on the order of a hash map.
    fn name(&self) -> Option<&str> {
        let mut tally: Vec<(&str, u32)> = Vec::new();
        for (spelling, count) in &self.names {
            let core = self
                .names
                .keys()
                .filter(|m| {
                    spelling.ends_with(m.as_str())
                        && spelling.chars().count() > m.chars().count()
                        && spelling.chars().count() - m.chars().count() <= 3
                })
                .min_by_key(|m| m.chars().count())
                .map_or(spelling.as_str(), String::as_str);
            match tally.iter_mut().find(|(t, _)| *t == core) {
                Some((_, votes)) => *votes += count,
                None => tally.push((core, *count)),
            }
        }
        tally
            .into_iter()
            .max_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| a.0.chars().count().cmp(&b.0.chars().count()))
                    .then_with(|| b.0.cmp(a.0))
            })
            .map(|(n, _)| n)
    }

    /// The cumulative this row ended at, if this tracker watched it end.
    /// Used only to derive the next row's segment, so it is deliberately not
    /// a baseline value: on a numbered board a baseline is a comparison time,
    /// and on a randomized one it is a game that finished before the bot
    /// looked, which is a time but not one this row was seen to reach.
    fn settled_cumulative(&self) -> Option<i64> {
        self.recorded
    }
}

/// What a board is, as far as this configuration is concerned.
pub enum Verdict<'a> {
    /// The board named nothing this configuration knows — no title was read,
    /// or the title was one no `[[games]]` entry and no `game.name` claims.
    /// Not evidence that the board changed.
    Silent,
    /// A board tracked by completions.
    Board(&'a GameAlias),
    /// A board this configuration knows and tracks some other way: the
    /// configured game, or an alias left in the default "runs" mode.
    Other,
}

/// Which `[[games]]` entry a board's title names, and whether that entry asks
/// for completion tracking.
///
/// A key `canonical_key` minted from a title nothing claims is `Silent`, not
/// `Other`. On the marathon board the title row goes unread often enough that
/// the reader takes the first game's name for it — a real pass over a real
/// Arcathlon board came back titled "'King Kong" — and a marathon must not
/// end because one pass called the board by the name of a row.
pub fn classify<'a>(board: &Board, cfg: &'a Config) -> Verdict<'a> {
    let Some((name, _)) = crate::board::canonical_key(board, cfg) else {
        return Verdict::Silent;
    };
    if let Some(a) = cfg.games.iter().find(|a| a.name == name) {
        return match a.mode {
            GameMode::Board => Verdict::Board(a),
            GameMode::Runs => Verdict::Other,
        };
    }
    if name == cfg.game.name {
        return Verdict::Other;
    }
    Verdict::Silent
}

/// One marathon in progress: the ten slots of a board and what each has shown.
#[derive(Debug, Clone)]
pub struct Marathon {
    /// The event, and the category every run of it is filed under.
    category: String,
    slots: Vec<Slot>,
    /// Event numbers read out of the title ("Arcathlon #6"), by count.
    numbers: HashMap<u32, u32>,
    /// Cumulative times already in the database for this event, so a restart
    /// mid-event does not record a completion twice.
    known: Vec<i64>,
    /// Pane passes seen, for the log.
    passes: u32,
}

impl Marathon {
    pub fn new(category: String) -> Self {
        Marathon {
            category,
            slots: Vec::new(),
            numbers: HashMap::new(),
            known: Vec::new(),
            passes: 0,
        }
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    /// The cumulative times this event already has runs for. A completion
    /// whose cumulative is one of these was recorded by an earlier run of the
    /// bot over the same broadcast and is not recorded again: within one
    /// event the cumulative column is strictly increasing, so it identifies
    /// the completion exactly, where a name damaged differently by two OCR
    /// passes does not.
    pub fn seed(&mut self, cumulatives: &[i64]) {
        self.known = cumulatives.to_vec();
    }

    /// How the session is tagged: the event, with its number when the title
    /// prints one. The raw title is recorded as a `title` session event, so
    /// this is deliberately the stable key rather than whatever tesseract
    /// spelled "Arcathlon" this minute.
    pub fn tag(&self) -> String {
        match self
            .numbers
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
        {
            Some((n, _)) => format!("{} #{n}", self.category),
            None => self.category.clone(),
        }
    }

    /// The board's rows as this tracker has settled them, for the log: the
    /// name of each slot and whether it has been recorded.
    pub fn describe(&self) -> String {
        let names: Vec<String> = self
            .slots
            .iter()
            .map(|s| {
                let n = s.name().unwrap_or("?");
                match s.recorded {
                    Some(_) => format!("{n}*"),
                    None => n.to_string(),
                }
            })
            .collect();
        format!(
            "{} of {} rows recorded over {} passes: {}",
            self.slots.iter().filter(|s| s.recorded.is_some()).count(),
            self.slots.len(),
            self.passes,
            names.join(", ")
        )
    }

    /// One pane pass. `total_ms` is the marathon total as last read, when it
    /// was read recently; it decides whether a row that arrives with a time
    /// already in it has just been finished or was finished before the bot
    /// looked, and otherwise only ever rejects a candidate.
    pub fn observe(&mut self, board: &Board, at_ms: i64, total_ms: Option<i64>) -> Vec<Completion> {
        self.passes += 1;
        if let Some(t) = board.title.as_deref() {
            if let Some(n) = event_number(t) {
                *self.numbers.entry(n).or_insert(0) += 1;
            }
        }
        let alignment = self.align(&board.rows);
        // Rows this pass adds to the board that already carry a time. One is
        // a game that has just been finished; ten at once is a numbered
        // board arriving with its ten comparison times. Rows without a time
        // do not count either way: a randomized board's first real row comes
        // in alongside the nine "???" placeholders under it, and counting
        // those made the tracker read a finished game as a comparison and
        // lose it for the whole event.
        let arriving = board
            .rows
            .iter()
            .zip(alignment.iter())
            .filter(|(row, slot)| {
                slot.is_some_and(|i| i >= self.slots.len())
                    && read_cells(row).cumulative_ms.is_some()
            })
            .count()
            <= 1;
        for (row, slot_idx) in board.rows.iter().zip(alignment.iter()) {
            let Some(i) = *slot_idx else { continue };
            while self.slots.len() <= i {
                self.slots.push(Slot::default());
            }
            if let Some(n) = clean_name(row.name.as_deref().unwrap_or_default()) {
                *self.slots[i].names.entry(n).or_insert(0) += 1;
            }
            let cells = read_cells(row);
            self.vote(i, cells, arriving, total_ms, at_ms);
        }
        self.harvest(at_ms, total_ms)
    }

    /// File one pass's reading of a row: first settle what the row showed
    /// before anything happened to it, then watch for that to change.
    ///
    /// `alone` says this pass brought at most one row the board did not have,
    /// which is what tells a game just finished from a board that arrived
    /// with its times already on it.
    fn vote(&mut self, i: usize, cells: Cells, alone: bool, total_ms: Option<i64>, at_ms: i64) {
        if self.slots[i].recorded.is_some() {
            return;
        }
        // A row that read one cell alone gave us the segment, not the
        // cumulative, and a row whose cells did not parse gave us nothing.
        // Neither is evidence about the row's state.
        let Some(observed) = cells.state() else {
            return;
        };
        let slot = &mut self.slots[i];
        if slot.baseline == Baseline::Unknown {
            // A randomized event's row does not exist until it has a time:
            // the board reader finds rows from their times, so the first game
            // of the day appears out of nothing with its result already in
            // it, and waiting for a row that "had no time and now has one"
            // would wait for ever. What says it just finished is the marathon
            // total standing at that very cumulative — a comparison time the
            // runner has not reached is minutes ahead of the total, and a
            // game finished before the bot looked is minutes behind it. Only
            // for a row that arrived on its own: a numbered board arrives all
            // at once, ten comparison times together, and those are baselines.
            if slot.baseline_votes.is_empty()
                && alone
                && observed.is_some_and(|c| just_now(c, total_ms))
            {
                slot.baseline = Baseline::Empty;
            } else {
                let v = slot.baseline_votes.entry(observed).or_insert(0);
                *v += 1;
                if *v >= AGREE {
                    slot.baseline = match observed {
                        Some(ms) => Baseline::Was(ms),
                        None => Baseline::Empty,
                    };
                }
                return;
            }
        }
        let Some(cum) = observed else { return };
        // The row is showing what it always showed. A real completion is
        // permanent — a completed row keeps its time for the rest of the
        // event — so whatever else this row read in between was the number
        // being misread, and none of it counts towards a change.
        if slot.baseline == Baseline::Was(cum) {
            slot.cumulative_votes.clear();
            slot.segment_votes.clear();
            slot.segment_waits = 0;
            return;
        }
        let v = slot.cumulative_votes.entry(cum).or_insert(Vote {
            count: 0,
            first_ms: at_ms,
            last_ms: at_ms,
        });
        v.count += 1;
        v.last_ms = at_ms;
        if let Some(seg) = cells.segment_ms {
            *slot.segment_votes.entry((cum, seg)).or_insert(0) += 1;
        }
    }

    /// Every slot whose new cumulative is now believed, turned into a run.
    fn harvest(&mut self, at_ms: i64, total_ms: Option<i64>) -> Vec<Completion> {
        let mut out = Vec::new();
        for i in 0..self.slots.len() {
            if self.slots[i].recorded.is_some() {
                continue;
            }
            // The marathon total is what tells a result from a comparison
            // time, so without one there is no verdict to give. Waiting costs
            // nothing: the board keeps a completed row's time for the rest of
            // the event. Measured the hard way — on the one broadcast whose
            // timer read on 8% of frames, an unchecked candidate recorded a
            // row's comparison time (2:42:48) eight minutes into the day.
            let Some(total) = total_ms else { continue };
            // The best of the readings this row could be finished at — not
            // simply the most-voted one, which the guards would then throw
            // away, taking the row's real completion down with it: a row
            // still being played read its segment column as a cumulative
            // three times before its real cumulative appeared twice.
            let Some((cum, _)) = self.slots[i]
                .cumulative_votes
                .iter()
                .map(|(c, v)| (*c, *v))
                .filter(|(c, v)| {
                    v.count >= AGREE
                        && v.spread_ms() >= AGREE_SPREAD_MS
                        // A cumulative ahead of the total is a comparison time
                        // the runner has not reached, not a result.
                        && *c <= total + AHEAD_OF_TOTAL_MS
                        // The board's own arithmetic has to hold: the rows run
                        // in order down the board and each cumulative includes
                        // every row above it.
                        && self.coherent(i, *c)
                })
                .max_by_key(|(c, v)| (v.count, *c))
            else {
                continue;
            };
            // Recorded already, by an earlier run of the bot over this
            // broadcast.
            if self.known.contains(&cum) {
                self.slots[i].recorded = Some(cum);
                continue;
            }
            // A run has to be filed under a name. A completed row keeps its
            // name for the rest of the event, so waiting costs a pass.
            let Some(game) = self.slots[i].name().map(str::to_string) else {
                continue;
            };
            let Some((segment_ms, derived)) = self.segment_for(i, cum) else {
                continue;
            };
            self.slots[i].recorded = Some(cum);
            self.known.push(cum);
            out.push(Completion {
                slot: i,
                game,
                category: self.category.clone(),
                segment_ms,
                cumulative_ms: cum,
                started_at_ms: at_ms - segment_ms,
                ended_at_ms: at_ms,
                segment_derived: derived,
            });
        }
        out
    }

    /// The run's time. The row's own segment column says it; the difference
    /// between this row's cumulative and the previous game's says it too,
    /// because the marathon total pauses between games and so the columns are
    /// exactly consistent. Where they agree there is nothing to decide; where
    /// they do not, the segment column is being misread ("0:30" for "50:30"
    /// was measured on a live board) and another pass usually settles it.
    /// Returns None while it is worth waiting, and the difference with
    /// `derived` set once it is not.
    fn segment_for(&mut self, i: usize, cum: i64) -> Option<(i64, bool)> {
        // A segment longer than the cumulative it is part of is not a
        // reading of this row at all — a real board came back with 46:44
        // beside a cumulative of 12:45.
        let voted = self.slots[i]
            .segment_votes
            .iter()
            .filter(|((c, s), _)| *c == cum && *s <= cum)
            .max_by_key(|((_, s), n)| (*n, *s))
            .map(|((_, s), n)| (*s, *n));
        let expected = self.expected_segment(i, cum);
        match (voted, expected) {
            (Some((seg, n)), Some(exp)) => {
                if (seg - exp).abs() <= SEGMENT_SLACK_MS {
                    Some((seg, false))
                } else {
                    self.slots[i].segment_waits += 1;
                    (self.slots[i].segment_waits >= SEGMENT_PATIENCE && n >= AGREE)
                        .then_some((exp, true))
                }
            }
            // No previous game to check against: take the column as read,
            // once two passes agree on it.
            (Some((seg, n)), None) => (n >= AGREE).then_some((seg, false)),
            // The segment column never read; the cumulatives still say what
            // the run took.
            (None, Some(exp)) => {
                self.slots[i].segment_waits += 1;
                (self.slots[i].segment_waits >= SEGMENT_PATIENCE).then_some((exp, true))
            }
            (None, None) => None,
        }
    }

    /// Does a candidate cumulative sit where the board says it must? The
    /// games run in order down the board and each row's cumulative includes
    /// every row above it, so a candidate has to fall between the rows
    /// already recorded either side of it. A reading that does not is cells
    /// read off the wrong row, or a number damaged past recognition.
    fn coherent(&self, i: usize, cum: i64) -> bool {
        !self
            .slots
            .iter()
            .enumerate()
            .any(|(j, s)| match s.recorded {
                Some(other) if j < i => other >= cum,
                Some(other) if j > i => other <= cum,
                _ => false,
            })
    }

    /// What the row's segment must be if the previous game's cumulative is
    /// settled: the first row's segment is its cumulative, since the marathon
    /// total starts at zero.
    fn expected_segment(&self, i: usize, cum: i64) -> Option<i64> {
        let prev = match i {
            0 => 0,
            _ => self.slots[i - 1].settled_cumulative()?,
        };
        (cum > prev).then_some(cum - prev)
    }

    /// Which slot each row of this pass belongs to.
    ///
    /// A board is not scrolled — the ten rows are all on screen — so position
    /// from the top is the answer whenever the names bear it out, and the
    /// names are what catch a row that went unread in the MIDDLE, since every
    /// row under it has moved up one and lands on somebody else's game.
    ///
    /// A pass with fewer rows than the board has is therefore not
    /// automatically a shift: the pane's bottom edge picks up a line of the
    /// text under it now and then (one pass of a real broadcast returned
    /// eleven rows, the last of them "all"), and after that every honest
    /// ten-row pass is one short of the slots. Reading those as name-anchors
    /// only cost that broadcast six games for two hours, because the "???"
    /// rows carry no name to anchor. So position is tried either way, and it
    /// has to be both uncontradicted and positively supported by a row whose
    /// name is the game its slot has been carrying.
    fn align(&self, rows: &[BoardRow]) -> Vec<Option<usize>> {
        if rows.is_empty() {
            return Vec::new();
        }
        let positional: Vec<Option<usize>> = (0..rows.len()).map(Some).collect();
        let ok = !self.contradicts(rows, &positional)
            && (rows.len() >= self.slots.len() || self.supports(rows, &positional));
        if ok {
            return positional;
        }
        self.by_name(rows)
    }

    /// Does a mapping have at least one row on the slot that has been
    /// carrying that game? Without one, a short pass of unreadable rows would
    /// be laid over the board on nothing but hope.
    fn supports(&self, rows: &[BoardRow], mapping: &[Option<usize>]) -> bool {
        rows.iter().zip(mapping).any(|(row, slot)| {
            let Some(i) = slot else { return false };
            let (Some(read), Some(known)) = (
                row.name.as_deref().and_then(clean_name),
                self.slots.get(*i).and_then(Slot::name),
            ) else {
                return false;
            };
            game_matches(&read, known)
        })
    }

    /// Does a positional reading put a row on a slot that has been carrying a
    /// different game's name? One such row means the board is not the board
    /// we think it is, or a row above went unread on a pass that still
    /// returned ten rows.
    fn contradicts(&self, rows: &[BoardRow], mapping: &[Option<usize>]) -> bool {
        rows.iter().zip(mapping).any(|(row, slot)| {
            let Some(i) = slot else { return false };
            let (Some(read), Some(known)) = (
                row.name.as_deref().and_then(clean_name),
                self.slots.get(*i).and_then(Slot::name),
            ) else {
                return false;
            };
            !game_matches(&read, known)
        })
    }

    /// Place only the rows whose name names exactly one slot, keeping the
    /// order of the board: an anchor that would put a row above one already
    /// placed higher up is not an anchor at all.
    fn by_name(&self, rows: &[BoardRow]) -> Vec<Option<usize>> {
        let mut out = vec![None; rows.len()];
        let mut claims: Vec<Option<usize>> = Vec::with_capacity(rows.len());
        for row in rows {
            claims.push(
                row.name
                    .as_deref()
                    .and_then(clean_name)
                    .and_then(|n| self.unique_slot(&n)),
            );
        }
        // A slot claimed by two rows identifies neither.
        let mut last = None;
        for (i, claim) in claims.iter().enumerate() {
            let Some(j) = *claim else { continue };
            if claims.iter().filter(|c| **c == Some(j)).count() > 1 {
                continue;
            }
            if last.is_some_and(|l| j <= l) {
                continue;
            }
            out[i] = Some(j);
            last = Some(j);
        }
        out
    }

    /// The one slot whose settled name is this name. An exact reading wins
    /// outright: "Batman" and "Batman: ROTJ" were two of the ten games of one
    /// real event, and fuzzily each name matches both rows.
    fn unique_slot(&self, name: &str) -> Option<usize> {
        let key = |s: &str| crate::board::normalise_title(s);
        let exact: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name().is_some_and(|n| key(n) == key(name)))
            .map(|(i, _)| i)
            .collect();
        if exact.len() == 1 {
            return Some(exact[0]);
        }
        if !exact.is_empty() {
            return None;
        }
        let close: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.name().is_some_and(|n| game_matches(name, n)))
            .map(|(i, _)| i)
            .collect();
        (close.len() == 1).then(|| close[0])
    }
}

impl Cells {
    /// What this reading says about the row's state: `Some(None)` for a row
    /// with no time of its own, `Some(Some(ms))` for one with a cumulative,
    /// and None when the reading is no evidence either way.
    fn state(&self) -> Option<Option<i64>> {
        match (self.segment_ms, self.cumulative_ms) {
            (_, Some(c)) => Some(Some(c)),
            // A row that read exactly one time gave us the segment column,
            // never the cumulative — assuming otherwise files a game's
            // segment as its total.
            (Some(_), None) => None,
            (None, None) => Some(None),
        }
    }
}

/// The row's segment and cumulative: the last two cells, whatever else is in
/// front of them. A completed row of a numbered event prints a signed delta
/// first, and whether the delta clears the threshold varies from pass to
/// pass, so counting from the left is not stable. "-" is LiveSplit's
/// placeholder for a time it has not got and parses as nothing.
fn read_cells(row: &BoardRow) -> Cells {
    // A signed cell is the delta and never a time column. Taking the last
    // two cells regardless files the SEGMENT as the cumulative whenever the
    // delta reads and the cumulative does not — on one broadcast the last
    // row read "+2:23", "25:16" for three passes running while the game was
    // still going, and 25:16 outvoted the real cumulative when it came.
    let cells: Vec<&String> = row.cells.iter().skip_while(|c| is_signed(c)).collect();
    let n = cells.len();
    if n == 0 {
        return Cells {
            segment_ms: None,
            cumulative_ms: None,
        };
    }
    if n == 1 {
        return Cells {
            segment_ms: parse_time(cells[0]),
            cumulative_ms: None,
        };
    }
    Cells {
        segment_ms: parse_time(cells[n - 2]),
        cumulative_ms: parse_time(cells[n - 1]),
    }
}

/// A cell carrying a sign: "+1:23", "-21.2". LiveSplit's bare "-" placeholder
/// is not one — it stands for a time the board has not got.
fn is_signed(cell: &str) -> bool {
    let t = cell.trim();
    t.len() > 1
        && (t.starts_with('+') || t.starts_with('-'))
        && t[1..].chars().any(|c| c.is_ascii_digit())
}

/// A row's name with the time column's leftovers taken off it. The delta is
/// coloured and sits between the name and the segment, so when it reads it
/// often lands in the name ("Felix the Cat -21.2", "Ts Blaster Master +6 55")
/// and when the name and a time merge into one word it lands there too. The
/// name is returned as the board prints it — casing, apostrophes and all —
/// because it is what the run is filed under. None when what is left is not
/// a name: "???" reads "22?", "229", "a 22?", and a highlighted row often
/// loses its label to the highlight bar.
pub fn clean_name(raw: &str) -> Option<String> {
    let mut tokens: Vec<&str> = raw.split_whitespace().collect();
    // Junk the transparent background leaves in front of the name is a token
    // with no letter in it ("4 Batman: ROTJ", "| Jackal").
    while tokens
        .first()
        .is_some_and(|t| !t.chars().any(char::is_alphabetic))
        && tokens.len() > 1
    {
        tokens.remove(0);
    }
    while let Some(last) = tokens.last() {
        if is_delta_or_time(last) {
            tokens.pop();
            continue;
        }
        // A game's name may end in a number ("Gremlins 2", "TMNT 2"), so a
        // trailing number comes off only when it is the tail of a delta the
        // OCR split in two ("Jackal -36 1" is "Jackal", "-36.1").
        if tokens.len() >= 2
            && last.chars().all(|c| c.is_ascii_digit())
            && is_delta_or_time(tokens[tokens.len() - 2])
        {
            tokens.pop();
            tokens.pop();
            continue;
        }
        break;
    }
    // The pane's left border and the highlight bar's edge come through as a
    // stray mark on the first letter ("'King Kong 2"); no game's name opens
    // with punctuation, and leaving it there splits the row's votes between
    // two spellings of one name.
    let name = tokens
        .join(" ")
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_string();
    (name.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some(name)
}

/// Is this cumulative where the marathon total stands right now? At the
/// moment a game ends the two are the same number, and the total then sits
/// there while the runner draws and sets up the next game, so a pane pass a
/// minute later still finds them together. A comparison time still to be run
/// is ahead of the total; a game finished before the bot looked is behind it.
/// Without a total there is no opinion, and the caller keeps waiting.
fn just_now(cumulative_ms: i64, total_ms: Option<i64>) -> bool {
    total_ms
        .is_some_and(|t| cumulative_ms <= t + AHEAD_OF_TOTAL_MS && cumulative_ms + JUST_NOW_MS >= t)
}

/// A signed delta ("+6:55", "-21.2", "-212") or a time ("31:24").
fn is_delta_or_time(t: &str) -> bool {
    let signed = t.starts_with('+') || t.starts_with('-');
    if signed
        && t.len() > 1
        && t[1..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
    {
        return true;
    }
    time_shaped(t)
}

/// The event's number, from a title that prints one ("Arcathlon #6" -> 6).
fn event_number(title: &str) -> Option<u32> {
    let (_, rest) = title.split_once('#')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GameAlias;

    fn row(name: &str, cells: &[&str]) -> BoardRow {
        BoardRow {
            name: (!name.is_empty()).then(|| name.to_string()),
            cells: cells.iter().map(|c| c.to_string()).collect(),
            y: 0,
        }
    }

    fn board(title: Option<&str>, rows: Vec<BoardRow>) -> Board {
        Board {
            title: title.map(str::to_string),
            subtitle: None,
            counter: None,
            rows,
        }
    }

    /// A randomized event: undrawn rows print nothing, and a row that gains
    /// times is a finished game. Two passes have to agree before it counts.
    #[test]
    fn randomized_row_gaining_times_is_a_completion() {
        let mut m = Marathon::new("Arcathlon".into());
        let undrawn = || row("22?", &[]);
        let pass = |first: &[&str]| {
            board(
                Some("Randomized Arcathlon"),
                vec![
                    row("Astyanax", first),
                    row("King Kong 2", &[]),
                    undrawn(),
                    undrawn(),
                ],
            )
        };
        assert!(m.observe(&pass(&[]), 1_000, Some(0)).is_empty());
        assert!(m.observe(&pass(&[]), 61_000, Some(60_000)).is_empty());
        // One pass with the time is not enough.
        let seen = m.observe(&pass(&["21:48", "21:48"]), 121_000, Some(1_308_000));
        assert!(seen.is_empty(), "one reading is not a completion");
        let seen = m.observe(&pass(&["21:48", "21:48"]), 181_000, Some(1_308_000));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Astyanax");
        assert_eq!(seen[0].category, "Arcathlon");
        assert_eq!(seen[0].segment_ms, 1_308_000);
        assert_eq!(seen[0].cumulative_ms, 1_308_000);
        assert_eq!(seen[0].ended_at_ms, 181_000);
        assert_eq!(seen[0].started_at_ms, 181_000 - 1_308_000);
        assert!(!seen[0].segment_derived);
        // And never again, however long the row keeps its time.
        for t in 1..20 {
            assert!(m
                .observe(
                    &pass(&["21:48", "21:48"]),
                    181_000 + t * 60_000,
                    Some(2_000_000)
                )
                .is_empty());
        }
    }

    /// A numbered event prints comparison times from the first frame; only a
    /// CHANGE is a completion, and it can be five seconds.
    #[test]
    fn numbered_event_records_the_change_not_the_comparison() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |first: &[&str]| {
            board(
                Some("Arcathlon #6"),
                vec![
                    row("Felix the Cat", first),
                    row("Gremlins 2", &["11:12", "42:36"]),
                ],
            )
        };
        // The comparison, settled over two passes.
        assert!(m.observe(&pass(&["31:37", "31:37"]), 0, Some(0)).is_empty());
        assert!(m
            .observe(&pass(&["31:37", "31:37"]), 60_000, Some(60_000))
            .is_empty());
        // Comparison times of rows still to be run are never completions.
        assert!(m
            .observe(&pass(&["31:37", "31:37"]), 120_000, Some(120_000))
            .is_empty());
        let total = Some(31 * 60_000 + 42_000);
        assert!(m
            .observe(&pass(&["31:42", "31:42"]), 1_880_000, total)
            .is_empty());
        let seen = m.observe(&pass(&["31:42", "31:42"]), 1_940_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Felix the Cat");
        assert_eq!(seen[0].cumulative_ms, 31 * 60_000 + 42_000);
        assert_eq!(seen[0].segment_ms, 31 * 60_000 + 42_000);
    }

    /// The comparison time of a game not yet run is minutes ahead of the
    /// marathon total; a completion is at it.
    #[test]
    fn a_cumulative_far_ahead_of_the_total_is_not_a_completion() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |second: &[&str]| {
            board(
                Some("Arcathlon #6"),
                vec![
                    row("Felix the Cat", &["31:24", "31:24"]),
                    row("Gremlins 2", second),
                ],
            )
        };
        m.observe(&pass(&["11:12", "42:36"]), 0, Some(0));
        m.observe(&pass(&["11:12", "42:36"]), 60_000, Some(60_000));
        // The row changes to another comparison-shaped value while the
        // marathon total is still half an hour behind it.
        let total = Some(31 * 60_000);
        assert!(m
            .observe(&pass(&["11:20", "42:44"]), 120_000, total)
            .is_empty());
        assert!(m
            .observe(&pass(&["11:20", "42:44"]), 180_000, total)
            .is_empty());
        // Once the total reaches it, the same reading is a completion.
        let total = Some(42 * 60_000 + 44_000);
        let seen = m.observe(&pass(&["11:20", "42:44"]), 240_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Gremlins 2");
    }

    /// A row that goes unread shifts the rows below it; the names put them
    /// back. Nothing is filed against the wrong game.
    #[test]
    fn a_dropped_row_does_not_shift_the_rest() {
        let mut m = Marathon::new("Arcathlon".into());
        let full = board(
            Some("Arcathlon #3"),
            vec![
                row("Arkista's Ring", &["15:02", "15:02"]),
                row("Batman: ROTJ", &["17:07", "32:09"]),
                row("Bionic Commando", &["22:00", "54:10"]),
            ],
        );
        m.observe(&full, 0, Some(0));
        m.observe(&full, 60_000, Some(60_000));
        // A pass that lost the first row: without name matching, Batman's
        // comparison would look like a change to Arkista's Ring.
        let short = board(
            Some("Arcathlon #3"),
            vec![
                row("Batman: ROTJ", &["17:07", "32:09"]),
                row("Bionic Commando", &["22:00", "54:10"]),
            ],
        );
        assert!(m.observe(&short, 120_000, Some(120_000)).is_empty());
        assert!(m.observe(&short, 180_000, Some(180_000)).is_empty());
        assert!(m.observe(&short, 240_000, Some(240_000)).is_empty());
        // The real completion of row 1 still lands on row 1.
        let done = board(
            Some("Arcathlon #3"),
            vec![
                row("Arkista's Ring", &["16:24", "16:24"]),
                row("Batman: ROTJ", &["17:07", "32:09"]),
                row("Bionic Commando", &["22:00", "54:10"]),
            ],
        );
        let total = Some(16 * 60_000 + 24_000);
        m.observe(&done, 300_000, total);
        let seen = m.observe(&done, 360_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].slot, 0);
        assert_eq!(seen[0].game, "Arkista's Ring");
    }

    /// One pass's digit slip never becomes a run: the value that repeats wins.
    /// A board as the bot really meets it: two rows still to be played, the
    /// first completing, then the second, with one pass of the second row's
    /// cumulative slipping a digit. The slip never repeats, so it never wins.
    #[test]
    fn a_single_frame_digit_slip_is_not_recorded() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |first: &[&str], second: &[&str]| {
            board(
                Some("Randomized Arcathlon"),
                vec![row("Astyanax", first), row("SMB3 (Warpless)", second)],
            )
        };
        m.observe(&pass(&[], &[]), 0, Some(0));
        m.observe(&pass(&[], &[]), 60_000, Some(60_000));
        // Astyanax finishes: 21:48 into the marathon.
        let after_one = Some(21 * 60_000 + 48_000);
        m.observe(&pass(&["21:48", "21:48"], &[]), 120_000, after_one);
        let seen = m.observe(&pass(&["21:48", "21:48"], &[]), 180_000, after_one);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Astyanax");
        // SMB3 finishes at 1:29:33, one pass reading it "1:28:33".
        let total = Some(89 * 60_000 + 33_000);
        let done = |c: &str| pass(&["21:48", "21:48"], &["1:07:45", c]);
        assert!(m.observe(&done("1:28:33"), 240_000, total).is_empty());
        assert!(m.observe(&done("1:29:33"), 300_000, total).is_empty());
        let seen = m.observe(&done("1:29:33"), 360_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].cumulative_ms, 89 * 60_000 + 33_000);
        assert_eq!(seen[0].segment_ms, 67 * 60_000 + 45_000);
        assert!(!seen[0].segment_derived);
    }

    /// The segment column is misread more often than the cumulative one; the
    /// difference between cumulatives is the check, and eventually the answer.
    #[test]
    fn a_segment_that_contradicts_the_cumulatives_is_not_taken() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |first: &[&str], second: &[&str]| {
            board(
                Some("Randomized Arcathlon"),
                vec![row("Astyanax", first), row("King Kong 2", second)],
            )
        };
        m.observe(&pass(&[], &[]), 0, Some(0));
        m.observe(&pass(&[], &[]), 60_000, Some(60_000));
        // Row 0 completes first, so row 1's difference is known.
        let after_one = Some(21 * 60_000 + 48_000);
        m.observe(&pass(&["21:48", "21:48"], &[]), 120_000, after_one);
        let seen = m.observe(&pass(&["21:48", "21:48"], &[]), 180_000, after_one);
        assert_eq!(seen.len(), 1);
        // "4:24 26:12" misread as "44:24 26:12": 26:12 - 21:48 is 4:24, so the
        // segment column is wrong and is held back rather than recorded.
        let total = Some(26 * 60_000 + 12_000);
        let done = pass(&["21:48", "21:48"], &["44:24", "26:12"]);
        for t in 1..=4 {
            assert!(m.observe(&done, 180_000 + t * 60_000, total).is_empty());
        }
        let seen = m.observe(&done, 600_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].segment_ms, 4 * 60_000 + 24_000);
        assert!(seen[0].segment_derived);
    }

    /// What is already in the database is not recorded again after a restart.
    #[test]
    fn seeded_cumulatives_are_not_recorded_twice() {
        let mut m = Marathon::new("Arcathlon".into());
        m.seed(&[1_308_000]);
        let pass = board(
            Some("Randomized Arcathlon"),
            vec![row("Astyanax", &[]), row("King Kong 2", &[])],
        );
        m.observe(&pass, 0, Some(0));
        m.observe(&pass, 60_000, Some(60_000));
        let done = board(
            Some("Randomized Arcathlon"),
            vec![
                row("Astyanax", &["21:48", "21:48"]),
                row("King Kong 2", &[]),
            ],
        );
        m.observe(&done, 120_000, Some(1_308_000));
        assert!(m.observe(&done, 180_000, Some(1_308_000)).is_empty());
    }

    /// A "???" row that gains a name is the game being drawn, and the slot is
    /// the same slot.
    #[test]
    fn an_undrawn_row_gaining_a_name_keeps_its_slot() {
        let mut m = Marathon::new("Arcathlon".into());
        let before = board(
            Some("Randomized Arcathlon"),
            vec![row("Astyanax", &["21:48", "21:48"]), row("22?", &[])],
        );
        m.observe(&before, 0, Some(1_308_000));
        m.observe(&before, 60_000, Some(1_308_000));
        let after = board(
            Some("Randomized Arcathlon"),
            vec![
                row("Astyanax", &["21:48", "21:48"]),
                row("King Kong 2", &["4:24", "26:12"]),
            ],
        );
        let total = Some(26 * 60_000 + 12_000);
        m.observe(&after, 120_000, total);
        let seen = m.observe(&after, 180_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].slot, 1);
        assert_eq!(seen[0].game, "King Kong 2");
        assert_eq!(seen[0].segment_ms, 4 * 60_000 + 24_000);
    }

    /// The real shape of the first game of a randomized day: the board reader
    /// finds rows by their times, so until Astyanax finishes there are no
    /// rows at all, and then there is one, with 21:48 already in it. Measured
    /// on VOD 2858870362, where the row appears at t=1790 and the answer key
    /// puts the finish at 1800.
    #[test]
    fn the_first_row_of_a_randomized_day_arrives_with_its_time() {
        let mut m = Marathon::new("Arcathlon".into());
        let empty = board(Some("Randomized Arcathion"), vec![]);
        for t in 0..6 {
            assert!(m.observe(&empty, t * 60_000, Some(t * 60_000)).is_empty());
        }
        let done = board(
            Some("Randomized Arcathion"),
            vec![row("Astyanax", &["21:48", "21:48"])],
        );
        let total = Some(21 * 60_000 + 48_000);
        assert!(m.observe(&done, 360_000, total).is_empty());
        let seen = m.observe(&done, 420_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Astyanax");
        assert_eq!(seen[0].segment_ms, 21 * 60_000 + 48_000);
    }

    /// The same arrival on a numbered board is ten comparison times, not ten
    /// finished games: they come all at once, and none of them is at the
    /// total.
    #[test]
    fn a_whole_board_arriving_at_once_is_comparison_times() {
        let mut m = Marathon::new("Arcathlon".into());
        let full = board(
            Some("Arcathlon #3"),
            vec![
                row("Arkista's Ring", &["15:02", "15:02"]),
                row("Batman: ROTJ", &["17:07", "32:09"]),
                row("Bionic Commando", &["22:00", "54:10"]),
            ],
        );
        for t in 0..6 {
            assert!(m.observe(&full, t * 60_000, Some(t * 60_000)).is_empty());
        }
    }

    /// A row already carrying a time that the total left behind long ago was
    /// finished before the bot looked; it is not this session's to record.
    #[test]
    fn a_row_the_total_has_left_behind_is_not_recorded() {
        let mut m = Marathon::new("Arcathlon".into());
        let done = board(
            Some("Randomized Arcathion"),
            vec![row("Astyanax", &["21:48", "21:48"])],
        );
        // Joined an hour in: the total is way past Astyanax's 21:48.
        let total = Some(70 * 60_000);
        for t in 0..6 {
            assert!(m.observe(&done, t * 60_000, total).is_empty());
        }
    }

    /// A row that was already finished when the board came into view, read
    /// off VOD 2827296024 pass by pass: its 23:19 comes back "23:10" on two
    /// consecutive ten-second passes and 23:19 on every other. Two readings
    /// ten seconds apart are one observation of the same pixels, and a real
    /// completion never goes back to the value it replaced.
    #[test]
    fn a_misread_that_repeats_on_the_next_pass_is_still_a_misread() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |c: &str| {
            board(
                Some("Arcathion #4"),
                vec![
                    row("Astyanax", &["23:19", c]),
                    row("Castlevania II", &["46:59", "1:10:19"]),
                ],
            )
        };
        // The marathon is well past this row: he is playing game two.
        let total = Some(31 * 60_000);
        let seq = [
            (0, "23:19"),
            (10_000, "23:19"),
            (20_000, "23:19"),
            (30_000, "23:10"),
            (40_000, "23:10"),
            (50_000, "23:19"),
            (60_000, "23:19"),
            (70_000, "23:19"),
        ];
        for (t, c) in seq {
            assert!(
                m.observe(&pass(c), t, total).is_empty(),
                "nothing was finished at {t}"
            );
        }
        // Even if the slip came back much later, the row has shown its real
        // value in between and the count starts again.
        for t in 8..14 {
            assert!(m.observe(&pass("23:19"), t * 60_000, total).is_empty());
        }
        assert!(m.observe(&pass("23:10"), 840_000, total).is_empty());
        assert!(m.observe(&pass("23:19"), 900_000, total).is_empty());
        assert!(m.observe(&pass("23:10"), 960_000, total).is_empty());
    }

    /// The marathon total is the whole basis for telling a result from a
    /// comparison time, so a pass that could not read it records nothing —
    /// and the pass that can, later, records everything, because the board
    /// keeps a completed row's time.
    #[test]
    fn nothing_is_recorded_without_a_marathon_total() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |c: &[&str]| {
            board(
                Some("Arcathlon #4"),
                vec![
                    row("Astyanax", &["23:19", "23:19"]),
                    row("Castlevania II", c),
                ],
            )
        };
        m.observe(&pass(&["46:59", "1:10:19"]), 0, None);
        m.observe(&pass(&["46:59", "1:10:19"]), 60_000, None);
        // The row changes, over and over, with no total to judge it by.
        for t in 2..8 {
            assert!(m
                .observe(&pass(&["47:10", "1:10:30"]), t * 60_000, None)
                .is_empty());
        }
        // The timer comes back, standing where the board says the game ended.
        let total = Some(70 * 60_000 + 30_000);
        let seen = m.observe(&pass(&["47:10", "1:10:30"]), 480_000, total);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Castlevania II");
    }

    /// Rows run in order down the board, so a cumulative that would land
    /// above a row already recorded above it is cells read off the wrong row.
    #[test]
    fn a_cumulative_out_of_order_with_the_recorded_rows_is_refused() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |first: &[&str], second: &[&str]| {
            board(
                Some("Randomized Arcathlon"),
                vec![row("Astyanax", first), row("King Kong 2", second)],
            )
        };
        m.observe(&pass(&[], &[]), 0, Some(0));
        m.observe(&pass(&[], &[]), 60_000, Some(60_000));
        let after_one = Some(21 * 60_000 + 48_000);
        m.observe(&pass(&["21:48", "21:48"], &[]), 120_000, after_one);
        assert_eq!(
            m.observe(&pass(&["21:48", "21:48"], &[]), 180_000, after_one)
                .len(),
            1
        );
        // Row 1 reads a cumulative EARLIER than the row above it, which the
        // board's own ordering rules out.
        let bad = pass(&["21:48", "21:48"], &["4:24", "12:45"]);
        for t in 4..10 {
            assert!(m.observe(&bad, t * 60_000, Some(1_400_000)).is_empty());
        }
    }

    /// A pass that calls the board by the name of one of its rows — which a
    /// real pass did, titling an Arcathlon board "'King Kong" — is not
    /// evidence that the board changed.
    #[test]
    fn a_title_nothing_claims_does_not_end_a_marathon() {
        let mut cfg = Config::for_test_with_min_final(660_000);
        cfg.game.name = "Ninja Gaiden (NES)".into();
        cfg.games = vec![GameAlias {
            name: "Arcathlon".into(),
            category: Some("10 games".into()),
            r#match: vec!["arcath".into(), "randomized".into()],
            mode: GameMode::Board,
        }];
        assert!(matches!(
            classify(&board(Some("\u{2018}King Kong"), vec![]), &cfg),
            Verdict::Silent
        ));
        // A board the configuration does know still ends it.
        assert!(matches!(
            classify(&board(Some("Ninja Gaiden (NES"), vec![]), &cfg),
            Verdict::Other
        ));
    }

    // ---- whole broadcasts, replayed pass by pass -------------------------
    //
    // A fixture is every pane pass of one real marathon, as the board reader
    // read it, with the marathon total the timer last gave within the
    // previous 30 s — exactly what the run loop hands `observe` — and the
    // ten games of that day from a hand-verified answer key. Synthetic tests
    // say what the rules are; these say the rules survive a whole broadcast
    // of the OCR damage a real board takes.

    #[derive(serde::Deserialize)]
    struct Fixture {
        name: String,
        passes: Vec<FPass>,
        expect: Vec<FGame>,
    }

    #[derive(serde::Deserialize)]
    struct FPass {
        t_ms: i64,
        total_ms: Option<i64>,
        title: Option<String>,
        rows: Vec<FRow>,
    }

    #[derive(serde::Deserialize)]
    struct FRow {
        name: Option<String>,
        cells: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct FGame {
        order: usize,
        game: String,
        segment: String,
        cumulative: String,
        ended_s: i64,
    }

    fn replay(name: &str) -> (Vec<Completion>, Vec<FGame>) {
        let path = format!(
            "{}/tests/fixtures/marathon/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let fx: Fixture = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        let mut m = Marathon::new("Arcathlon".into());
        let mut out = Vec::new();
        for p in &fx.passes {
            let b = Board {
                title: p.title.clone(),
                subtitle: None,
                counter: None,
                rows: p
                    .rows
                    .iter()
                    .map(|r| BoardRow {
                        name: r.name.clone(),
                        cells: r.cells.clone(),
                        y: 0,
                    })
                    .collect(),
            };
            out.extend(m.observe(&b, p.t_ms, p.total_ms));
        }
        println!("{}: {}", fx.name, m.describe());
        (out, fx.expect)
    }

    /// Every game of the day, with the time the board printed, and no game
    /// the board never finished. Names are compared the way a person would
    /// read them — OCR takes a letter off "Little Samson" and writes
    /// "Litle Samson" — so `game_matches` decides, and the exact spellings
    /// are printed for a human to look over.
    fn check(name: &str, want_found: usize, allow_late_s: i64) {
        let (found, expect) = replay(name);
        let mut bad: Vec<String> = Vec::new();
        let mut hits = 0;
        for g in &expect {
            let cum = parse_time(&g.cumulative).expect("answer key cumulative");
            let seg = parse_time(&g.segment).expect("answer key segment");
            let Some(c) = found.iter().find(|c| c.cumulative_ms == cum) else {
                println!(
                    "  {:2}  {:26} NOT RECORDED ({})",
                    g.order, g.game, g.cumulative
                );
                continue;
            };
            hits += 1;
            let late = c.ended_at_ms / 1000 - g.ended_s;
            println!(
                "  {:2}  {:26} {:26} {:>9} {:>9}  end {:+5}s{}",
                g.order,
                g.game,
                c.game,
                g.segment,
                format_ms_short(c.segment_ms),
                late,
                if c.segment_derived {
                    "  (segment from the cumulatives)"
                } else {
                    ""
                }
            );
            if !game_matches(&c.game, &g.game) {
                bad.push(format!("{} named {:?}, not {:?}", g.order, c.game, g.game));
            }
            if c.segment_ms != seg {
                bad.push(format!(
                    "{} timed {} ms, the board printed {}",
                    g.order, c.segment_ms, g.segment
                ));
            }
            if late.abs() > allow_late_s {
                bad.push(format!("{} recorded {late}s from the answer key", g.order));
            }
        }
        // Nothing recorded that the answer key does not have: a spurious run
        // is worse than a missing one.
        for c in &found {
            if !expect
                .iter()
                .any(|g| parse_time(&g.cumulative) == Some(c.cumulative_ms))
            {
                bad.push(format!(
                    "spurious {:?} at cumulative {} ms",
                    c.game, c.cumulative_ms
                ));
            }
        }
        assert!(bad.is_empty(), "{name}: {}", bad.join("; "));
        assert!(
            hits >= want_found,
            "{name}: only {hits} of {} games found",
            expect.len()
        );
    }

    fn format_ms_short(ms: i64) -> String {
        let s = ms / 1000;
        match s / 3600 {
            0 => format!("{}:{:02}", s / 60, s % 60),
            h => format!("{h}:{:02}:{:02}", (s / 60) % 60, s % 60),
        }
    }

    /// A randomized day: no comparison times, "???" until each game is drawn,
    /// and the first row appearing out of nothing with its result in it. All
    /// ten games, every time exactly as the board printed it, each recorded
    /// 50-80 s after the answer key's instant — the pane pass runs every
    /// minute and a completion wants two of them, and the key's own instants
    /// are good to 30 s.
    #[test]
    fn replays_a_whole_randomized_broadcast() {
        check("rand-2858870362", 10, 120);
    }

    /// A numbered day: ten comparison times from the first frame, over a
    /// transparent pane with the game showing through it, which is what makes
    /// the readings thinner. All ten games and every time exact here too, but
    /// the worst lag is 310 s (Jurassic Park) where the pane went unreadable
    /// for a few passes. That is the board's whole virtue: it is cumulative,
    /// so a stretch it cannot be read through delays a reading rather than
    /// destroying it.
    #[test]
    fn replays_a_whole_numbered_broadcast() {
        check("num-2830524439", 10, 360);
    }

    /// The pane's edge reads as a letter in front of the name more often than
    /// it does not, so the readings of one row have to be grouped before they
    /// are counted — but only where the difference is a smudge, not a word.
    #[test]
    fn a_letter_in_front_of_the_name_does_not_win_the_vote() {
        let mut m = Marathon::new("Arcathlon".into());
        // The real counts from VOD 2826325488's second row.
        let mut pass = |name: &str, times: usize| {
            for t in 0..times {
                m.observe(
                    &board(Some("Arcathion #3"), vec![row(name, &["17:07", "32:09"])]),
                    t as i64 * 60_000,
                    Some(0),
                );
            }
        };
        pass("a Batman: ROTJ", 62);
        pass("Batman: ROTJ", 49);
        pass("s Batman: ROTJ", 5);
        assert!(m.describe().contains("Batman: ROTJ"));
        assert!(!m.describe().contains("a Batman: ROTJ"));
        // A whole word in front is a different reading, not a smudge: the
        // board really does print "SMB3 (Warpless)", and a pass that lost
        // "SMB3" must not rename the game.
        let mut m2 = Marathon::new("Arcathlon".into());
        for t in 0..10 {
            let n = if t % 3 == 0 {
                "(Warpless)"
            } else {
                "SMB3 (Warpless)"
            };
            m2.observe(
                &board(
                    Some("Randomized Arcathion"),
                    vec![row(n, &["1:03:20", "1:29:33"])],
                ),
                t * 60_000,
                Some(0),
            );
        }
        assert!(m2.describe().contains("SMB3 (Warpless)"));
    }

    #[test]
    fn names_lose_the_delta_and_keep_the_game() {
        assert_eq!(
            clean_name("Felix the Cat -21.2").as_deref(),
            Some("Felix the Cat")
        );
        assert_eq!(
            clean_name("Felix the Cat -212").as_deref(),
            Some("Felix the Cat")
        );
        assert_eq!(clean_name("Jackal -36 1").as_deref(), Some("Jackal"));
        assert_eq!(
            clean_name("Ts Blaster Master +6 55").as_deref(),
            Some("Ts Blaster Master")
        );
        assert_eq!(
            clean_name("4 Batman: ROTJ").as_deref(),
            Some("Batman: ROTJ")
        );
        // A number that is part of the name stays part of the name.
        assert_eq!(clean_name("Gremlins 2").as_deref(), Some("Gremlins 2"));
        assert_eq!(clean_name("TMNT 2").as_deref(), Some("TMNT 2"));
        assert_eq!(
            clean_name("SMB3 (Warpless)").as_deref(),
            Some("SMB3 (Warpless)")
        );
        assert_eq!(
            clean_name("Chip 'n Dale 2").as_deref(),
            Some("Chip 'n Dale 2")
        );
        // Placeholders and fragments are not names.
        assert_eq!(clean_name("22?"), None);
        assert_eq!(clean_name("229"), None);
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name("a 22?"), None);
    }

    #[test]
    fn the_last_two_cells_are_the_segment_and_the_cumulative() {
        // A delta that cleared the threshold is a third cell in front.
        let c = read_cells(&row("Gremlins 2", &["-33.7", "11:12", "42:36"]));
        assert_eq!(c.segment_ms, Some(11 * 60_000 + 12_000));
        assert_eq!(c.cumulative_ms, Some(42 * 60_000 + 36_000));
        // One cell is the segment, never the cumulative.
        let c = read_cells(&row("Hebereke", &["12:23"]));
        assert_eq!(c.cumulative_ms, None);
        assert_eq!(
            c.state(),
            None,
            "one cell says nothing about the row's state"
        );
        // LiveSplit's placeholder is not a time.
        let c = read_cells(&row("Metal Storm", &["-", "-"]));
        assert_eq!(c.state(), Some(None));
    }

    #[test]
    fn the_tag_names_the_event_and_its_number() {
        let mut m = Marathon::new("Arcathlon".into());
        assert_eq!(m.tag(), "Arcathlon");
        let b = board(Some("Arcathion #6"), vec![]);
        m.observe(&b, 0, None);
        assert_eq!(m.tag(), "Arcathlon #6");
    }

    #[test]
    fn classify_reads_the_mode_off_the_alias() {
        let mut cfg = Config::for_test_with_min_final(660_000);
        cfg.game.name = "Ninja Gaiden (NES)".into();
        cfg.games = vec![GameAlias {
            name: "Arcathlon".into(),
            category: Some("10 games".into()),
            r#match: vec!["arcath".into()],
            mode: GameMode::Board,
        }];
        let marathon = board(Some("Randomized Arcathion"), vec![]);
        assert!(matches!(classify(&marathon, &cfg), Verdict::Board(a) if a.name == "Arcathlon"));
        assert!(matches!(
            classify(&board(Some("Ninja Gaiden (NES)"), vec![]), &cfg),
            Verdict::Other
        ));
        assert!(matches!(
            classify(&board(None, vec![]), &cfg),
            Verdict::Silent
        ));
        // The default mode leaves the board alone.
        cfg.games[0].mode = GameMode::Runs;
        assert!(matches!(classify(&marathon, &cfg), Verdict::Other));
    }
}
