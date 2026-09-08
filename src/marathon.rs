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
//! - **The marathon total is a reading too.** It is what tells a result from
//!   a comparison time — the two are the same number at the moment a game
//!   ends — but the big timer goes illegible like anything else on screen, and
//!   tesseract does not stop answering when it does: it parses the wreckage
//!   into numbers, minutes or hours from the truth and different every frame.
//!   Three broadcasts recorded four games of ten that way. So a completion
//!   needs the total OR the board's own arithmetic — this row's cumulative
//!   less the row above's, against this row's segment column — and where the
//!   tracker has watched the row above finish, the board is the better
//!   witness: it is static text, cumulative, and it keeps a finished row's
//!   time for the rest of the event.
//!
//! Nothing in this module talks to the database or the clock: `observe` takes
//! a board and returns the completions to record, which is what makes it
//! testable against boards copied out of a real broadcast.

use std::collections::HashMap;

use crate::board::{game_matches, Board, BoardRow};
use crate::config::{Config, GameAlias, GameMode};
use crate::signature::{BoardSignature, Shape};
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

/// What the board's own arithmetic says a candidate cumulative's segment
/// must be. The three answers are different evidence and are kept apart:
/// "the difference is this", "the previous game ended after this one, so the
/// candidate is not a reading of this row", and "there is no settled row
/// above to check against".
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Segment(i64),
    Impossible,
    Unanchored,
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

/// What kind of board this is, and which `[[games]]` entry it belongs to.
///
/// The **rows** decide what it is, not the title. A marathon board is
/// different games over a running total; a run board is one game's segments
/// counting up. That is what [`crate::signature::BoardSignature`] measures,
/// and it survives damage that destroys the title outright: a pass over a
/// real Arcathlon board came back titled "'King Kong", because the title
/// row went unread and the reader took the first game's name for it, and a
/// marathon must not end because one pass called the board by the name of
/// one of its rows.
///
/// The **title** only says which event it is, so the runs are filed under
/// the right category. When it names no configured entry — unread, or read
/// as a game's name — and exactly one entry tracks boards, that entry is
/// used anyway: there is nothing to be ambiguous about. It is also allowed
/// to START a marathon on its own, when the rows cannot yet speak: the
/// first game of a randomized day is on the board ALONE when it completes
/// (the board reader finds rows by their times, and the rest are "???"), so
/// there are not three names to measure, and a board that reads as nothing
/// while its title names the event is that event.
///
/// What the title names, it names for both sides. A title that names THIS
/// deployment's own game is the one thing that can outweigh a marathon
/// reading of the rows, because a run board really can measure like a
/// marathon: his six "Act" rows plus "Previous Segment" and "Sum of Best
/// Segments" come back with two or three of the labels damaged into words
/// of their own, the group of matching labels falls under two in three, and
/// the board reads as different games. Measured on the eight marathon
/// broadcasts: on two of them, passes of his NINJA GAIDEN board after the
/// event read as a marathon board, and one of them recorded "ct) A" — Act 1
/// — as a finished game of 47.4 s. So a marathon-shaped board whose title
/// names a game this configuration tracks some other way is that game's.
///
/// `current` is the marathon in force, and it is what a run-shaped reading
/// is checked against. Two of his games can be spelled close enough to
/// collapse into one label — "Ninja Gaiden III" and "Ninja Gaiden Il" on one
/// real board — and three named rows then measure as one game's segments,
/// for twenty passes running on the broadcast that has both. A board that
/// still names the event's own games cannot be somebody else's board,
/// whatever it measures like, so it is [`Verdict::Silent`] and the marathon
/// stands. Only a board that names none of them may end one.
///
/// An unreadable board is [`Verdict::Silent`] and changes nothing, which is
/// the safe answer: a marathon in force stays in force, and none is started
/// unless the title says which. Only a board that reads clearly as one
/// game's segments ends a marathon.
pub fn classify<'a>(board: &Board, cfg: &'a Config, current: Option<&Marathon>) -> Verdict<'a> {
    let boards: Vec<&GameAlias> = cfg
        .games
        .iter()
        .filter(|a| a.mode == GameMode::Board)
        .collect();
    // What the title names, when it names anything this configuration knows:
    // `canonical_key` falls back to the title itself, which names nothing.
    let named = crate::board::canonical_key(board, cfg).map(|(n, _)| n);
    let titled = || -> Option<&'a GameAlias> {
        let name = named.as_deref()?;
        boards.iter().find(|a| a.name == name).copied()
    };
    // The title names a game this deployment tracks some other way: its own
    // game, or an alias left in the default "runs" mode.
    let titled_elsewhere = || -> bool {
        named.as_deref().is_some_and(|n| {
            n == cfg.game.name
                || cfg
                    .games
                    .iter()
                    .any(|a| a.name == n && a.mode != GameMode::Board)
        })
    };
    // Somebody else's board — unless the marathon in force still has its
    // rows on it, in which case this pass says nothing at all.
    let somebody_else = || match current {
        Some(m) if m.claims(board) => Verdict::Silent,
        _ => Verdict::Other,
    };
    match BoardSignature::of(board).shape() {
        Shape::Marathon { .. } => {
            // Which event? The title, when it names one of them; else the
            // only entry that tracks boards, if there is exactly one.
            if let Some(a) = titled() {
                return Verdict::Board(a);
            }
            if titled_elsewhere() {
                return somebody_else();
            }
            match boards.as_slice() {
                [only] => Verdict::Board(only),
                _ => Verdict::Silent,
            }
        }
        // One game's segments: whatever this deployment tracks by its timer,
        // and the thing that ends a marathon when he goes back to it — but
        // never while the board still carries the marathon's own rows.
        Shape::Run { .. } => somebody_else(),
        // The rows cannot say what this is. The title still can, and it is
        // positive evidence: it may start a marathon, though it may never
        // end one — that is the rows' to say.
        Shape::Unknown => match titled() {
            Some(a) => Verdict::Board(a),
            None => Verdict::Silent,
        },
    }
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

    /// Does this board still carry rows of this marathon? One row named
    /// after a game the event is running is enough: the board reader damages
    /// a name or drops a row every pass or so, and a board that names one of
    /// these games is this board, whatever its rows measure like.
    ///
    /// This is what stops a run-shaped reading of the marathon's own board
    /// from ending the event. Only slots that have settled on a name can
    /// answer, which is right: a marathon with nothing read off it yet has
    /// nothing to be mistaken for.
    pub fn claims(&self, board: &Board) -> bool {
        board.rows.iter().any(|r| {
            let Some(read) = r.name.as_deref().and_then(clean_name) else {
                return false;
            };
            self.slots
                .iter()
                .filter_map(Slot::name)
                .any(|known| game_matches(&read, known))
        })
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
        // A row already carrying a time an earlier run of the bot recorded.
        // Nothing else identifies it: the row shows what it always showed
        // from the moment this tracker starts, so it never reaches
        // `harvest`, where `known` is otherwise applied.
        let already = observed.is_some_and(|c| self.known.contains(&c));
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
            //
            // Any reading while the baseline is still unsettled, not just the
            // row's first: a baseline is what the row showed BEFORE anything
            // happened to it, and a reading standing where the total stands is
            // not that. One real row's 15:52 came back "18:52" on every other
            // pass, and taking the first sighting alone let the misreading
            // settle as the baseline and held the game up for eighteen
            // minutes — the misreading is minutes ahead of the total, and this
            // is exactly the test that tells them apart.
            if alone && observed.is_some_and(|c| just_now(c, total_ms)) {
                slot.baseline = Baseline::Empty;
            } else {
                let v = slot.baseline_votes.entry(observed).or_insert(0);
                *v += 1;
                if *v >= AGREE {
                    slot.baseline = match observed {
                        Some(ms) => Baseline::Was(ms),
                        None => Baseline::Empty,
                    };
                    // A baseline that is a cumulative already in the
                    // database is a row this bot finished watching before it
                    // restarted. Marking it recorded here is the only chance
                    // there is: a completed row keeps its time, so the
                    // baseline is all this tracker will ever see of it, and
                    // the only reading that could reach `harvest` is a
                    // MISREADING of it — which is how a restart mid-event
                    // recorded a second run of a finished game. It also puts
                    // the recorded prefix back, which is what the segment
                    // and coherence cross-checks are made of.
                    if already {
                        slot.recorded = observed;
                    }
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
                        // The board's own arithmetic has to hold: the rows run
                        // in order down the board and each cumulative includes
                        // every row above it.
                        && self.coherent(i, *c)
                        // And something has to say the runner is HERE, at this
                        // cumulative, rather than somewhere else on the board:
                        // either the marathon total, or the row's own two
                        // columns against the row above it.
                        && (self.total_agrees(i, *c, total_ms) || self.board_vouches(i, *c))
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
            // The board's arithmetic ruled the candidate out. Never take the
            // segment column for it: "no previous game to check against" and
            // "the previous game ended after this one" are opposite answers,
            // and reading them as one is what let a segment column be
            // recorded as a cumulative.
            (_, Expect::Impossible) => None,
            (Some((seg, n)), Expect::Segment(exp)) => {
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
            (Some((seg, n)), Expect::Unanchored) => (n >= AGREE).then_some((seg, false)),
            // The segment column never read; the cumulatives still say what
            // the run took.
            (None, Expect::Segment(exp)) => {
                self.slots[i].segment_waits += 1;
                (self.slots[i].segment_waits >= SEGMENT_PATIENCE).then_some((exp, true))
            }
            (None, Expect::Unanchored) => None,
        }
    }

    /// Does the board's own arithmetic vouch for a candidate the marathon
    /// total has left far behind? It does when the row above this one is one
    /// THIS tracker watched finish and the candidate comes after it: the
    /// completion continues a chain the tracker built, which is what a game
    /// confirmed a few passes late looks like — the pane can go unreadable
    /// for minutes, and the fixtures hold completions recorded 490 s after
    /// the fact. A number read off the wrong column has no such chain: on a
    /// mid-event join nothing above the row is recorded, and that is exactly
    /// where a segment column was taken for a cumulative.
    fn arithmetic_backs(&self, i: usize, cum: i64) -> bool {
        matches!(self.expected_segment(i, cum), Expect::Segment(_))
    }

    /// Does the marathon total put the runner at this cumulative?
    ///
    /// At the moment a game ends the two are the same number, and the total
    /// then sits there while he draws and sets up the next one. A comparison
    /// time he has not reached is minutes AHEAD of it; a game that ended
    /// before the bot looked is minutes BEHIND it — the case that needs the
    /// lower bound is joining an event mid-way, where nothing above the row is
    /// recorded and so nothing else constrains a number read off the wrong
    /// column: VOD 2827296024's Cowboy Kid row prints "-2.35", "28:03",
    /// "1:38:23", the pane loses the minus on many passes, and 28:03 — the
    /// SEGMENT column — was recorded as a cumulative with 2.35 s as the game's
    /// time while the total stood at 3:57:30. Where the tracker has watched
    /// the row above finish, that chain constrains it instead.
    ///
    /// No total, no opinion. The total is the only witness that can speak for
    /// a row the board's arithmetic cannot reach, and an unread one says
    /// nothing rather than yes: measured on the one broadcast whose timer read
    /// on 8% of frames, where an unchecked candidate recorded a row's
    /// comparison time (2:42:48) eight minutes into the day.
    fn total_agrees(&self, i: usize, cum: i64, total_ms: Option<i64>) -> bool {
        let Some(total) = total_ms else {
            return false;
        };
        cum <= total + AHEAD_OF_TOTAL_MS
            && (cum + JUST_NOW_MS >= total || self.arithmetic_backs(i, cum))
    }

    /// Does the board speak for a candidate on its own, with the marathon
    /// total left out of it?
    ///
    /// A cumulative board is one equation per row: this row's cumulative is
    /// the row above's plus this row's segment, and the board prints both
    /// sides. So when the row above is one THIS tracker watched finish — a
    /// cumulative it measured, not a comparison time it found already
    /// printed — and the candidate's own segment column is that difference to
    /// the second, the candidate is not a number read off the wrong column or
    /// a digit slipped in the cumulative: it continues a chain the tracker
    /// built, and the row's other column agrees. Both columns would have to be
    /// damaged in exactly compensating ways to fake it, on two passes a minute
    /// apart, which is not what OCR damage looks like.
    ///
    /// That matters because the marathon total is a reading too, and it fails
    /// the way readings fail. When the big timer goes illegible tesseract does
    /// not stop answering — it parses the wreckage into numbers, minutes or
    /// hours from the truth and different every frame — and a total like that
    /// vetoes every completion of the rest of the day. The board is the better
    /// witness there: it is static text, cumulative, and it keeps a finished
    /// row's time for the rest of the event.
    ///
    /// The first row is deliberately not covered. `expected_segment` answers
    /// `cum` for it, since the marathon starts at zero, and the segment column
    /// of a first row prints exactly that — so the equation is vacuously true
    /// and says nothing. There the total is the only witness there is.
    fn board_vouches(&self, i: usize, cum: i64) -> bool {
        if i == 0 {
            return false;
        }
        let Expect::Segment(exp) = self.expected_segment(i, cum) else {
            return false;
        };
        // Two readings of the segment column, the same standard the cumulative
        // itself is held to. `segment_for` will take a single reading that
        // agrees with the arithmetic, because by then the completion is
        // already believed and only its time is in question; here the
        // completion is what is in question, so one reading is not enough.
        self.slots[i]
            .segment_votes
            .iter()
            .any(|((c, s), n)| *c == cum && *n >= AGREE && (*s - exp).abs() <= SEGMENT_SLACK_MS)
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
    fn expected_segment(&self, i: usize, cum: i64) -> Expect {
        let prev = match i {
            0 => 0,
            _ => match self.slots[i - 1].settled_cumulative() {
                Some(p) => p,
                None => return Expect::Unanchored,
            },
        };
        match cum > prev {
            true => Expect::Segment(cum - prev),
            false => Expect::Impossible,
        }
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
    /// rows carry no name to anchor. So position is tried either way, and
    /// where the pass is short it has to be positively supported by a row
    /// whose name is the game its slot has been carrying.
    fn align(&self, rows: &[BoardRow]) -> Vec<Option<usize>> {
        if rows.is_empty() {
            return Vec::new();
        }
        let positional: Vec<Option<usize>> = (0..rows.len()).map(Some).collect();
        let ok = !self.shifted(rows)
            && (rows.len() >= self.slots.len() || self.supports(rows, &positional));
        if ok {
            return positional;
        }
        self.by_name(rows)
    }

    /// Have the rows moved up — a row unread higher in the pane pushing every
    /// one under it onto the next game — or is a row simply sitting on a slot
    /// whose name was never read properly?
    ///
    /// A shift says so itself: the row that does not match its own slot
    /// matches one FURTHER DOWN, because that is where its game sits. A slot
    /// that took a bad name off one early pass ("Previous Segment" and worse
    /// come through where the pane's footer meets the last row) also fails to
    /// match, and vetoing the whole pass for it is a trap with no way out —
    /// the row is never placed, so the slot never learns the real name, so
    /// the row is never placed. That cost one broadcast its last three games.
    fn shifted(&self, rows: &[BoardRow]) -> bool {
        rows.iter().enumerate().any(|(i, row)| {
            let Some(read) = row.name.as_deref().and_then(clean_name) else {
                return false;
            };
            let mismatched = self
                .slots
                .get(i)
                .and_then(Slot::name)
                .is_some_and(|here| !game_matches(&read, here));
            mismatched
                && self
                    .slots
                    .iter()
                    .skip(i + 1)
                    .filter_map(Slot::name)
                    .any(|below| game_matches(&read, below))
        })
    }

    /// Does a mapping have at least one row on the slot that has been
    /// carrying that game? Without one, a short pass of unreadable rows would
    /// be laid over the board on nothing but hope.
    ///
    /// Unless no slot has a name at all, which is where a randomized day
    /// starts: he opens one broadcast with the previous event's splits still
    /// loaded, ten rows of "???" over last week's times, and the pane then
    /// shows only the row he is playing. There is nothing to support a
    /// placing with and nothing to contradict it either, and the rows a
    /// short pass does return are the ones that have times, which on this
    /// board are the ones at the top.
    fn supports(&self, rows: &[BoardRow], mapping: &[Option<usize>]) -> bool {
        if self.slots.iter().all(|s| s.name().is_none()) {
            return true;
        }
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

    /// Where the board cannot speak for a row itself, the marathon total is
    /// the whole basis for telling a result from a comparison time — and here
    /// it cannot: the row above was finished before this tracker started, so
    /// nothing above the candidate is a cumulative it measured. A pass that
    /// could not read the timer therefore records nothing, and the pass that
    /// can, later, records everything, because the board keeps a completed
    /// row's time.
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

    /// The timer is a reading, and it fails the way readings fail: when the
    /// big clock goes illegible tesseract does not stop answering, it parses
    /// the wreckage. So the marathon total arrives minutes or hours from the
    /// truth and different every pass, and it vetoes every completion left in
    /// the day — four of ten games on VOD 2839800169, four of ten again on
    /// 2816723472 and 2852473277.
    ///
    /// The board is the better witness there. Once the tracker has watched the
    /// row above finish, the next row's cumulative and its own segment column
    /// are one equation with a number the tracker measured on the other side
    /// of it, and a row that satisfies it is finished whatever the timer says.
    #[test]
    fn the_board_speaks_for_a_completion_a_broken_timer_would_veto() {
        // VOD 2839800169's first two rows: Castlevania III's comparison
        // 34:41 becoming his 41:23, then Duck Tales' 42:54 becoming 49:45 —
        // and 49:45 - 41:23 is the 8:22 the board rounds to the 8:21 it
        // prints in the segment column.
        let pass = |first: &[&str], second: &[&str]| {
            board(
                Some("Arcathion #5"),
                vec![row("Castlevania III", first), row("Duck Tales", second)],
            )
        };
        // Readings the run loop really handed the tracker after that day's
        // timer went illegible, in the order it gave them.
        let wreckage = [5_058_i64, 155_888_000, 3_558_020];
        let first_done = Some(2_483_000);
        let run = |totals: [Option<i64>; 2]| -> Vec<Completion> {
            let mut m = Marathon::new("Arcathlon".into());
            for t in 0..2 {
                m.observe(
                    &pass(&["34:41", "34:41"], &["8:13", "42:54"]),
                    t * 60_000,
                    Some(t * 60_000),
                );
            }
            // Row 0 finishes while the timer still reads.
            m.observe(
                &pass(&["41:23", "41:23"], &["8:13", "42:54"]),
                120_000,
                first_done,
            );
            let seen = m.observe(
                &pass(&["41:23", "41:23"], &["8:13", "42:54"]),
                180_000,
                first_done,
            );
            assert_eq!(seen.len(), 1, "row 0 with a working timer");
            // Then row 1 finishes, with whatever the timer has become.
            let done = pass(&["41:23", "41:23"], &["8:21", "49:45"]);
            let seen = m.observe(&done, 240_000, totals[0]);
            assert!(seen.is_empty(), "one reading is still not a completion");
            m.observe(&done, 300_000, totals[1])
        };
        // A total hours behind the truth, then one hours ahead of it: either
        // one on its own vetoes the completion, and the board overrules both.
        for pair in [
            [Some(wreckage[0]), Some(wreckage[1])],
            [Some(wreckage[1]), Some(wreckage[2])],
            // And no reading at all, which is how a broadcast that ends while
            // the pane is unreadable loses its last game — VOD 2844298651's
            // Wizards and Warriors, VOD 2856167316's Mega Man 4.
            [None, None],
        ] {
            let seen = run(pair);
            assert_eq!(seen.len(), 1, "the board vouches for it: {seen:?}");
            assert_eq!(seen[0].game, "Duck Tales");
            assert_eq!(seen[0].segment_ms, 501_000);
            assert_eq!(seen[0].cumulative_ms, 2_985_000);
            assert!(!seen[0].segment_derived);
        }
    }

    /// What the board vouching for a row may not become: a way past the
    /// guards. It speaks only where the row ABOVE is a cumulative this tracker
    /// watched reach its end, and only where this row's own segment column is
    /// that difference — the two damaged in exactly compensating ways is not
    /// what OCR damage looks like.
    #[test]
    fn the_board_vouches_only_for_a_row_it_can_do_the_arithmetic_for() {
        // A mid-event join: the rows above were finished before this tracker
        // started, so they are baselines, not measurements. VOD 2827296024's
        // Cowboy Kid row, whose lost minus sign files its SEGMENT as a
        // cumulative, is refused here for ever however well the columns fit.
        let mut m = Marathon::new("Arcathlon".into());
        let joined = |third: &[&str]| {
            board(
                Some("Arcathion #4"),
                vec![
                    row("Astyanax", &["23:19", "23:19"]),
                    row("Castlevania II", &["46:59", "1:10:19"]),
                    row("Cowboy Kid", third),
                ],
            )
        };
        let total = Some(14_250_000);
        for t in 0..2 {
            assert!(m
                .observe(&joined(&["-2.35", "28:03", "1:38:23"]), t * 60_000, total)
                .is_empty());
        }
        for t in 2..10 {
            assert!(
                m.observe(&joined(&["2.35", "28:03"]), t * 60_000, total)
                    .is_empty(),
                "nothing above this row is one the tracker measured, at {t}"
            );
        }
        // And a row whose two columns do not agree: the cumulative says this
        // game took 8:22, its own segment column says 40 minutes. The board is
        // vouching for nothing, so only the timer could speak, and it is the
        // wreckage a dead timer parses to.
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |first: &[&str], second: &[&str]| {
            board(
                Some("Arcathion #5"),
                vec![row("Castlevania III", first), row("Duck Tales", second)],
            )
        };
        for t in 0..2 {
            m.observe(
                &pass(&["34:41", "34:41"], &["8:13", "42:54"]),
                t * 60_000,
                Some(t * 60_000),
            );
        }
        let first_done = Some(2_483_000);
        let after_one = pass(&["41:23", "41:23"], &["8:13", "42:54"]);
        m.observe(&after_one, 120_000, first_done);
        assert_eq!(m.observe(&after_one, 180_000, first_done).len(), 1);
        let mismatched = pass(&["41:23", "41:23"], &["40:00", "49:45"]);
        for t in 4..12 {
            assert!(
                m.observe(&mismatched, t * 60_000, Some(5_058)).is_empty(),
                "the columns disagree, so only the timer could speak, at {t}"
            );
        }
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

    /// A marathon board whose title came back as one of its own rows — which
    /// a real pass did, titling an Arcathlon board "'King Kong" — is still a
    /// marathon board, because the rows say so and the title is only asked
    /// which event it is.
    #[test]
    fn the_rows_say_what_the_board_is_when_the_title_cannot() {
        let cfg = board_config();
        let marathon_rows = || {
            vec![
                row("Astyanax", &["21:48", "21:48"]),
                row("King Kong 2", &["4:24", "26:12"]),
                row("SMB3 (Warpless)", &["1:03:20", "1:29:33"]),
                row("Batman: ROTJ", &["15:43", "1:45:16"]),
            ]
        };
        // The title is useless; the rows are not.
        assert!(matches!(
            classify(&board(Some("\u{2018}King Kong"), marathon_rows()), &cfg, None),
            Verdict::Board(a) if a.name == "Arcathlon"
        ));
        // No title at all: the same, since only one entry tracks boards.
        assert!(matches!(
            classify(&board(None, marathon_rows()), &cfg, None),
            Verdict::Board(a) if a.name == "Arcathlon"
        ));
        // One game's segments end it, whatever the title says.
        let acts: Vec<BoardRow> = (0..6)
            .map(|i| {
                row(
                    &format!("Act {}", i + 1),
                    &["1:00.0", &format!("{}:00.0", (i + 1) * 2)],
                )
            })
            .collect();
        assert!(matches!(
            classify(&board(Some("\u{2018}King Kong"), acts), &cfg, None),
            Verdict::Other
        ));
        // A board with nothing readable changes nothing.
        assert!(matches!(
            classify(&board(Some("Ninja Gaiden (NES"), vec![]), &cfg, None),
            Verdict::Silent
        ));
    }

    /// A marathon board can read as a run board, and did for twenty passes
    /// running. Two of the day's ten games were "Ninja Gaiden III" and
    /// "Ninja Gaiden II"; two edits apart, the signature groups them as one
    /// damaged label, and three named rows with two in one group are one
    /// game's segments. A board still carrying the marathon's own rows may
    /// not end it, whatever it measures like.
    #[test]
    fn a_run_shaped_reading_of_the_marathons_own_board_does_not_end_it() {
        let cfg = board_config();
        // VOD 2833684629, t=2950 s, verbatim.
        let pass = || {
            let mut rows = vec![
                row("Ninja Gaiden III", &["15:52", "15:52"]),
                row("SMB 2", &["11:25", "27:18"]),
                row("Ninja Gaiden Il", &[]),
            ];
            rows.extend((0..7).map(|_| row("22?", &[])));
            board(Some("Randomized Arcathion"), rows)
        };
        assert!(
            matches!(BoardSignature::of(&pass()).shape(), Shape::Run { .. }),
            "the signature really does read this marathon board as a run board"
        );
        // With no marathon in force, that reading stands: nothing is claimed.
        assert!(matches!(classify(&pass(), &cfg, None), Verdict::Other));
        // With one whose rows these are, the board changes nothing — and the
        // rows are still read, which is how the day's third game gets
        // recorded at all.
        let mut m = Marathon::new("Arcathlon".into());
        m.observe(&pass(), 0, Some(1_278_930));
        assert!(matches!(classify(&pass(), &cfg, Some(&m)), Verdict::Silent));
        // The board he really does switch to at the end of the day names
        // none of them, and that ends it.
        let acts: Vec<BoardRow> = (0..6)
            .map(|i| {
                row(
                    &format!("Act {}", i + 1),
                    &["1:00.0", &format!("{}:00.0", (i + 1) * 2)],
                )
            })
            .collect();
        assert!(matches!(
            classify(&board(Some("Ninja Gaiden (NES)"), acts), &cfg, Some(&m)),
            Verdict::Other
        ));
    }

    /// The first game of a randomized day finishes while its row is the only
    /// one on the board — the others are "???" and the board reader finds
    /// rows by their times — so there are not three names for the signature
    /// to measure and the rows cannot yet say what the board is. The title
    /// can, and it is allowed to START a marathon on that evidence. It is
    /// never allowed to end one: only the rows do that.
    #[test]
    fn the_title_starts_a_marathon_the_rows_cannot_speak_for() {
        let cfg = board_config();
        let alone = |title: Option<&str>| board(title, vec![row("Astyanax", &["21:48", "21:48"])]);
        assert!(matches!(
            BoardSignature::of(&alone(Some("Randomized Arcathion"))).shape(),
            Shape::Unknown
        ));
        assert!(matches!(
            classify(&alone(Some("Randomized Arcathion")), &cfg, None),
            Verdict::Board(a) if a.name == "Arcathlon"
        ));
        // Without a title naming one, an unreadable board is still no
        // evidence about anything.
        assert!(matches!(
            classify(&alone(None), &cfg, None),
            Verdict::Silent
        ));
        // A run board never starts a marathon, however the title reads.
        let acts: Vec<BoardRow> = (0..6)
            .map(|i| {
                row(
                    &format!("Act {}", i + 1),
                    &["1:00.0", &format!("{}:00.0", (i + 1) * 2)],
                )
            })
            .collect();
        assert!(matches!(
            classify(&board(Some("Randomized Arcathion"), acts), &cfg, None),
            Verdict::Other
        ));
        // And the point of all this: started in time, the completion is
        // recognised. Measured on VOD 2858870362, where the row appears at
        // t=1790 with the marathon total standing at its 21:48.
        let mut m = Marathon::new("Arcathlon".into());
        let total = Some(1_309_320);
        let seen = alone(Some("Randomized Arcathion"));
        assert!(m.observe(&seen, 1_790_000, total).is_empty());
        let done = m.observe(&seen, 1_850_000, total);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].game, "Astyanax");
        assert_eq!(done[0].segment_ms, 1_308_000);
    }

    /// A restart mid-event: the rows already finished carry their times from
    /// the first pass this tracker sees, so they never CHANGE and never
    /// reach the point where what is already in the database is checked.
    /// The only reading of such a row that can get that far is a misreading
    /// — VOD 2833684629's row 1 reads "18:52" for 15:52 on every other pass
    /// — and it lands as a second finished run of a game already recorded,
    /// at a time the runner never posted.
    #[test]
    fn a_restart_does_not_record_a_finished_row_again() {
        let mut m = Marathon::new("Arcathlon".into());
        // Ninja Gaiden III, 15:52, recorded before the restart.
        m.seed(&[952_000]);
        let pass = |first: &[&str]| {
            board(
                Some("Randomized Arcathion"),
                vec![
                    row("Ninja Gaiden III", first),
                    row("SMB 2", &["11:25", "27:18"]),
                    row("Ninja Gaiden II", &[]),
                ],
            )
        };
        // He is deep into the third game: the total has left both finished
        // rows far behind, which is what a restart mid-event looks like.
        let total = Some(3_600_000);
        // Two passes settle the row's baseline at its real, recorded value.
        for t in 0..2 {
            assert!(m
                .observe(&pass(&["15:52", "15:52"]), t * 60_000, total)
                .is_empty());
        }
        // The documented misreading, twice, a minute apart — enough votes and
        // enough spread — and then the pane thins to the row being played and
        // the row's cells go unread, which does not clear the votes.
        for t in 2..4 {
            let seen = m.observe(&pass(&["15:52", "18:52"]), t * 60_000, total);
            assert!(seen.is_empty(), "at {t}: {seen:?}");
        }
        for t in 4..9 {
            assert!(
                m.observe(&pass(&[]), t * 60_000, total).is_empty(),
                "a row already in the database is not recorded again at {t}"
            );
        }
    }

    /// Joining an event mid-way — a crash restart, a rollout, a stream
    /// reconnect — is the ordinary live case, and there nothing above the
    /// row being played has been recorded, so nothing constrains a number
    /// read off the wrong column. VOD 2827296024's Cowboy Kid row prints
    /// "-2.35", "28:03", "1:38:23"; the pane loses the leading minus on many
    /// passes, the last two cells become the delta and the SEGMENT, and
    /// 28:03 was recorded as the cumulative with 2.35 s as the game's time —
    /// while the marathon total stood at 3:57:30.
    #[test]
    fn a_cumulative_hours_behind_the_total_is_not_a_completion() {
        let mut m = Marathon::new("Arcathlon".into());
        let pass = |third: &[&str], fourth: &[&str]| {
            board(
                Some("Arcathion #4"),
                vec![
                    row("Astyanax", &["23:19", "23:19"]),
                    row("Castlevania II", &["46:59", "1:10:19"]),
                    row("Cowboy Kid", third),
                    row("Shadowgate", fourth),
                ],
            )
        };
        let total = Some(14_250_000);
        let read = &["-2.35", "28:03", "1:38:23"][..];
        for t in 0..2 {
            assert!(m.observe(&pass(read, &[]), t * 60_000, total).is_empty());
        }
        // The minus goes, over and over. It is not a change in the row.
        for t in 2..10 {
            assert!(
                m.observe(&pass(&["2.35", "28:03"], &[]), t * 60_000, total)
                    .is_empty(),
                "nothing was finished at {t}"
            );
        }
        // The game he is playing still records the moment it ends, which is
        // what the bound must not cost: its cumulative is AT the total.
        let done = &["13:32", "4:11:02"][..];
        let end = Some(15_062_000);
        assert!(m.observe(&pass(read, done), 600_000, end).is_empty());
        let seen = m.observe(&pass(read, done), 660_000, end);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].game, "Shadowgate");
        assert_eq!(seen[0].segment_ms, 812_000);
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
        /// The spelling the tracker is expected to file the run under, where
        /// OCR does not give the board's own — "Litle Samson" for "Little
        /// Samson". Absent means it reads the name exactly. `runs.game` is
        /// grouped by exact string, so this is checked exactly: a change in
        /// what a row settles on is a change in where its history goes.
        #[serde(default)]
        recorded_as: Option<String>,
        segment: String,
        cumulative: String,
        ended_s: i64,
    }

    impl FGame {
        fn filed_as(&self) -> &str {
            self.recorded_as.as_deref().unwrap_or(&self.game)
        }
    }

    /// A board that is plainly a marathon's: four different games over a
    /// running total.
    fn marathon_rows() -> Vec<BoardRow> {
        vec![
            row("Astyanax", &["21:48", "21:48"]),
            row("King Kong 2", &["4:24", "26:12"]),
            row("SMB3 (Warpless)", &["1:03:20", "1:29:33"]),
            row("Batman: ROTJ", &["15:43", "1:45:16"]),
        ]
    }

    /// The configuration the fixtures are replayed against: one entry that
    /// tracks boards, which is what a deployment following this streamer has.
    fn board_config() -> Config {
        let mut cfg = Config::for_test_with_min_final(660_000);
        cfg.game.name = "Ninja Gaiden (NES)".into();
        cfg.games = vec![GameAlias {
            name: "Arcathlon".into(),
            category: Some("10 games".into()),
            r#match: vec!["arcath".into(), "randomized".into()],
            mode: GameMode::Board,
        }];
        cfg
    }

    /// One pane pass, as the run loop hands it over.
    struct Pass {
        at_ms: i64,
        total_ms: Option<i64>,
        board: Board,
    }

    /// Drive passes through the decision sequence `app::track_marathon`
    /// runs — `classify`, the take-up and let-go counts, the database
    /// reconcile, then `observe` — rather than straight into `observe`.
    /// Feeding `observe` directly validates a path the bot does not run: it
    /// cannot see a marathon that is never started, or one torn down
    /// mid-event, which is what two of the fixtures do. No database is
    /// needed; a `Vec<i64>` of the cumulatives recorded so far is exactly
    /// what `db::marathon_totals` returns.
    ///
    /// `restart_at` drops everything the tracker has learned at that pass and
    /// picks the event up again from the database alone, which is what a
    /// crash restart, a rollout or a stream reconnect does to the live bot
    /// mid-event — the ordinary case, and not the one a replay from the
    /// first pass exercises.
    fn drive(passes: &[Pass], restart_at: Option<usize>) -> (Vec<Completion>, String) {
        let cfg = board_config();
        let mut state: Option<Marathon> = None;
        let mut misses: u32 = 0;
        let mut hits: u32 = 0;
        // The database: every cumulative recorded for this event so far.
        let mut recorded: Vec<i64> = Vec::new();
        let mut out = Vec::new();
        let mut verdicts = [0u32; 3];
        for (n, p) in passes.iter().enumerate() {
            if restart_at == Some(n) {
                // The process goes down and comes back. Everything it had
                // learned about the board goes with it; the database stays.
                state = None;
                misses = 0;
                hits = 0;
            }
            match classify(&p.board, &cfg, state.as_ref()) {
                // Not evidence of anything: a marathon in force keeps
                // reading the board, and none is started.
                Verdict::Silent => verdicts[0] += 1,
                Verdict::Other => {
                    verdicts[1] += 1;
                    misses += 1;
                    hits = 0;
                    // Somebody else's board — but the rows are left alone
                    // until enough passes agree, and then the event is over.
                    if misses >= crate::app::MARATHON_LET_GO {
                        state = None;
                    }
                    continue;
                }
                Verdict::Board(alias) => {
                    verdicts[2] += 1;
                    misses = 0;
                    if state.as_ref().is_none_or(|m| m.category() != alias.name) {
                        // And an event is taken up on several passes
                        // agreeing, the way it is let go.
                        hits += 1;
                        if hits < crate::app::MARATHON_TAKE_UP {
                            continue;
                        }
                        let mut m = Marathon::new(alias.name.clone());
                        m.seed(&recorded);
                        state = Some(m);
                    }
                }
            }
            let Some(m) = state.as_mut() else { continue };
            for c in m.observe(&p.board, p.at_ms, p.total_ms) {
                recorded.push(c.cumulative_ms);
                out.push(c);
            }
        }
        let summary = format!(
            "{} passes ({} board, {} silent, {} other){} — {}",
            passes.len(),
            verdicts[2],
            verdicts[0],
            verdicts[1],
            match restart_at {
                Some(n) => format!(", restarted at pass {n}"),
                None => String::new(),
            },
            match &state {
                Some(m) => m.describe(),
                None => "no marathon in force at the end".to_string(),
            }
        );
        (out, summary)
    }

    fn replay_from(name: &str, restart_at: Option<usize>) -> (Vec<Completion>, Vec<FGame>) {
        let path = format!(
            "{}/tests/fixtures/marathon/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let fx: Fixture = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
        let passes: Vec<Pass> = fx
            .passes
            .iter()
            .map(|p| Pass {
                at_ms: p.t_ms,
                total_ms: p.total_ms,
                board: Board {
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
                },
            })
            .collect();
        let (out, summary) = drive(&passes, restart_at);
        println!("{}: {summary}", fx.name);
        (out, fx.expect)
    }

    /// Every game of the day, with the time the board printed, and no game
    /// the board never finished.
    ///
    /// The name is compared EXACTLY, against the spelling the answer key
    /// says this row settles on. Runs are filed under the row's own name and
    /// `runs.game` groups by exact string, so a fuzzy comparison here would
    /// pass a change that silently splits a game's history in two.
    fn check(name: &str, want_found: usize, allow_late_s: i64) {
        check_from(name, None, want_found, allow_late_s)
    }

    fn check_from(name: &str, restart_at: Option<usize>, want_found: usize, allow_late_s: i64) {
        let (found, expect) = replay_from(name, restart_at);
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
            if c.game != g.filed_as() {
                bad.push(format!(
                    "{} filed under {:?}, not {:?}",
                    g.order,
                    c.game,
                    g.filed_as()
                ));
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
    /// and the first row appearing out of nothing with its result in it. Every
    /// time exactly as the board printed it, and every name exactly as the
    /// runs are filed.
    ///
    /// NINE of its ten games, not ten, and the missing one is the point of
    /// the fixture rather than a fault in it. This capture's pane crop cut
    /// the title row off, so the board reader took the first game's name for
    /// the title or read none at all — and Astyanax finishes while its row is
    /// the ONLY row on the board, before there are three names for the
    /// signature to measure. The rows cannot say what the board is, the title
    /// is not there to say it either, so no marathon is in force during the
    /// nine passes where the total stands at Astyanax's 21:48, and by the
    /// time three games are drawn the total has moved four minutes past it.
    /// King Kong 2 is recorded 290 s late for the same reason. Replayed whole
    /// with the crop `scripts/replay-arcathlon.sh` uses, which takes in the
    /// title row, this broadcast records all ten.
    #[test]
    fn replays_a_whole_randomized_broadcast() {
        check("rand-2858870362", 9, 300);
    }

    /// The hardest of the three, and the one that pays for the other two.
    /// He opens this broadcast with the PREVIOUS event's splits still loaded
    /// — ten rows of "???" over last week's times — so every slot starts with
    /// a baseline and a nameless row; the pane then shows only the row he is
    /// playing, so passes come back with one row where the board has ten; one
    /// row's 15:52 reads "18:52" on every other pass; and the pane's footer
    /// leaks in as an eleventh row often enough to leave a slot carrying junk
    /// for a name. All ten games, every time exact, the worst 490 s late
    /// where the pane went thin.
    #[test]
    fn replays_a_whole_broadcast_that_starts_on_the_last_one() {
        check("rand-2833684629", 10, 600);
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

    /// A numbered day whose big timer went illegible an hour and three
    /// quarters in and never came back — and tesseract went on answering,
    /// parsing the wreckage into "5.058", "9.699", "0499", numbers minutes or
    /// hours from the marathon total and different every frame.
    ///
    /// Every completion after the fourth game read as either ahead of that
    /// total or hours behind it, and the day recorded four games of ten. It is
    /// the case for reading the board's own arithmetic as evidence in its own
    /// right: the six lost games are all vouched for by the row above them and
    /// their own segment column, to the second, on a board that says nothing
    /// at all about the timer.
    ///
    /// Its last game is the tightest of them: Vice: Project Doom's row shows
    /// its result on the last two passes of the broadcast and on no others,
    /// which is exactly the two readings a minute apart that a completion
    /// needs, with the timer reading 1:30:05 for a marathon four and a half
    /// hours old.
    #[test]
    fn replays_a_broadcast_whose_timer_died_halfway() {
        check("num-2839800169", 10, 400);
    }

    /// The same broadcast with the bot restarted in the middle of it — a
    /// crash, a rollout, a stream reconnect, all of which the live bot does
    /// mid-event and none of which a replay from the first pass exercises.
    /// The rows already finished when it comes back are marked from the
    /// database as it reconciles, so the day still records its ten games and
    /// no eleventh.
    ///
    /// Before that reconcile was applied where a restart can see it, row 1's
    /// documented 15:52 -> "18:52" misread landed as a second, fabricated
    /// run of a game already in the database: measured on this fixture at
    /// every restart from pass 70 to pass 210.
    #[test]
    fn replays_a_broadcast_the_bot_restarted_in_the_middle_of() {
        check_from("rand-2833684629", Some(150), 10, 600);
    }

    /// A run board can measure like a MARATHON board, which is the mirror
    /// of the case above and the more dangerous one: it starts an event
    /// where there is none. His Ninja Gaiden board is six "Act" rows over
    /// "Previous Segment" and "Sum of Best Segments", and when two or three
    /// of the labels come back damaged into words of their own, the group of
    /// matching labels falls under two in three and the rows read as
    /// different games. Two of the eight marathon broadcasts have passes
    /// like that AFTER the event, and one of them recorded Act 1 as a
    /// finished game of 47.4 s under the name "ct) A".
    ///
    /// What settles it is the title: it names the game this deployment
    /// tracks by its timer, and that outranks a marathon reading of damaged
    /// rows.
    #[test]
    fn a_run_board_that_measures_like_a_marathon_is_not_one() {
        let cfg = board_config();
        // VOD 2833684629, t=17350 s, verbatim.
        let b = board(
            Some("Ninje Ga n (NES)"),
            vec![
                row("Acti", &["0:47.5", "0:47.5"]),
                row("", &["1:54.3", "2:41.9"]),
                row("1 Act 3", &["1:22.6", "4:04.6"]),
                row("Act 4 19", &["2:11.4", "6:16.0"]),
                row("REF", &["2:24.7", "8:38.8"]),
                row("Act6", &["2:58.3", "11:37.2"]),
                row("5 Previous Segment", &["0.1"]),
                row("Sum of Best Segments", &["11:32.5"]),
            ],
        );
        assert!(
            matches!(BoardSignature::of(&b).shape(), Shape::Marathon { .. }),
            "the signature really does read his run board as a marathon board"
        );
        assert!(matches!(classify(&b, &cfg, None), Verdict::Other));
        // The same board with its title unread is the one this cannot save,
        // and it is why board mode tracks whatever it is pointed at: with a
        // single board-mode entry the rows are all there is to go on.
        let untitled = board(None, b.rows.clone());
        assert!(matches!(classify(&untitled, &cfg, None), Verdict::Board(_)));
    }

    /// Two consecutive passes of his run board reading as a marathon board
    /// is the most the eight broadcasts ever produced, and an event taken up
    /// on them files acts as games and suspends the timer. So an event is
    /// taken up the way it is let go: on three passes agreeing.
    ///
    /// The five passes are VOD 2830524439 at t=18190..18430 s, verbatim,
    /// half an hour after the marathon ended and while he was running Ninja
    /// Gaiden. Three of them read as a marathon board — their labels are
    /// damaged into words of their own and stop matching each other — and on
    /// an earlier build they started an event that recorded "Act 1" as a
    /// finished game of 47.5 s. The two whose titles still read as the
    /// tracked game are what breaks the run of them.
    #[test]
    fn an_event_is_taken_up_only_when_three_passes_agree() {
        let ng = |title: Option<&str>, rows: Vec<BoardRow>| Pass {
            at_ms: 0,
            total_ms: Some(700_000),
            board: board(title, rows),
        };
        let mut passes = vec![
            ng(
                Some("Ninja Gaiden (NES)"),
                vec![
                    row("", &["-0.1", "0:47.4", "0:47.4"]),
                    row("Act 2", &["1:54.2", "2:41.6"]),
                    row("1 Act 3", &["1:30.6", "4:12.2"]),
                    row("ir ate", &["2:11.5", "6:14.1"]),
                    row("Act", &["2:24.7", "8:38.8"]),
                    row("", &["2:58.3", "11:37.2"]),
                    row("Sum of Best Segments", &["11:32.5"]),
                ],
            ),
            ng(
                Some("Set ay i ony"),
                vec![
                    row("ea", &["0:47.5", "0:47.5"]),
                    row("Act 2", &["1:53.8", "2:41.3"]),
                    row("Act 3", &["1:21.1", "4:02.5"]),
                    row("4", &["2:11.5", "6:14.1"]),
                    row("act", &["2:24.7", "8:38.8"]),
                    row("Act 6", &["2:58.3", "11:37.2"]),
                    row("Previous Segment", &[]),
                    row("Sum of Best Segments", &["11:32.5"]),
                ],
            ),
            ng(
                Some("Gesen (NES)"),
                vec![
                    row("Act 1", &["0:47.5", "0:47.5"]),
                    row("Act 2", &["1:53.8", "2:41.3"]),
                    row("A", &["1:21.1", "4:02.5"]),
                    row("gy Acts", &["2:11.5", "6:14.1"]),
                    row("Act", &["2:24.7", "8:38.8"]),
                    row("Act 8", &["2:58.3", "11:37.2"]),
                    row("Previous Segment", &[]),
                    row("Sum of Best Segments", &["11:32.5"]),
                ],
            ),
            ng(
                Some("Ninja Garden (NES)"),
                vec![
                    row("", &["0:47.5", "0:47.5"]),
                    row("a] Act 2", &["1:53.8", "2:41.3"]),
                    row("4", &["1:21.1", "4:02.5"]),
                    row("", &["2:11.5", "6:14.1"]),
                    row("Act 5", &["2:24.7", "8:38.8"]),
                    row("a Act6", &["2:58.3", "11:37.2"]),
                    row("l", &[]),
                    row("q", &[]),
                    row("Previous Segment", &[]),
                    row("Sum of Best Segments", &["11:32.5"]),
                ],
            ),
            ng(
                Some("Ninge Gan Ge on (NES)"),
                vec![
                    row("Act 1", &["0:47.5", "0:47.5"]),
                    row("", &["1:53.8", "2:41.3"]),
                    row("Act3", &["1:21.1", "4:02.5"]),
                    row("", &["2:11.5", "6:14.1"]),
                    row("Act", &["2:24.7", "8:38.8"]),
                    row("", &["2:58.3", "11:37.2"]),
                    row("Previous Segment", &[]),
                    row("1 Sum of Best Segments", &["11:32.5"]),
                ],
            ),
        ];
        for (i, p) in passes.iter_mut().enumerate() {
            p.at_ms = 18_190_000 + i as i64 * 60_000;
        }
        // Two of the five read as a marathon board, and they are adjacent.
        // Three of the five read as a marathon board — two of them
        // adjacent, which is the longest run the eight broadcasts produced.
        let verdicts: Vec<bool> = passes
            .iter()
            .map(|p| matches!(classify(&p.board, &board_config(), None), Verdict::Board(_)))
            .collect();
        assert_eq!(verdicts, [false, true, true, false, true]);
        let (found, summary) = drive(&passes, None);
        assert!(found.is_empty(), "nothing was finished here: {found:?}");
        assert!(
            summary.contains("no marathon in force"),
            "no event should have been taken up: {summary}"
        );
        // The same passes with a third agreeing between them do take one up,
        // which is what a real marathon board looks like for hours.
        passes.insert(2, ng(Some("Randomized Arcathion"), marathon_rows()));
        passes[2].at_ms = 18_310_000;
        let (_, summary) = drive(&passes, None);
        assert!(
            !summary.contains("no marathon in force"),
            "three agreeing passes take the event up: {summary}"
        );
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
        let marathon = || {
            board(
                Some("Randomized Arcathion"),
                vec![
                    row("Astyanax", &["21:48", "21:48"]),
                    row("King Kong 2", &["4:24", "26:12"]),
                    row("Hebereke", &["28:19", "54:31"]),
                ],
            )
        };
        assert!(
            matches!(classify(&marathon(), &cfg, None), Verdict::Board(a) if a.name == "Arcathlon")
        );
        // One game's segments are somebody else's board.
        let acts: Vec<BoardRow> = (0..6)
            .map(|i| {
                row(
                    &format!("Act {}", i + 1),
                    &["1:00.0", &format!("{}:00.0", (i + 1) * 2)],
                )
            })
            .collect();
        assert!(matches!(
            classify(&board(Some("Ninja Gaiden (NES)"), acts), &cfg, None),
            Verdict::Other
        ));
        assert!(matches!(
            classify(&board(None, vec![]), &cfg, None),
            Verdict::Silent
        ));
        // The same board with its entry left in the default mode: the
        // configuration knows this event and says to track it by the timer,
        // which is what `Other` means. Nothing can start a marathon here,
        // and a board nothing claims still changes nothing.
        cfg.games[0].mode = GameMode::Runs;
        assert!(matches!(classify(&marathon(), &cfg, None), Verdict::Other));
        assert!(matches!(
            classify(
                &board(Some("Some Other Stream"), marathon().rows),
                &cfg,
                None
            ),
            Verdict::Silent
        ));
    }
}
