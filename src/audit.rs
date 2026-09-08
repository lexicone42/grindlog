//! Does what the tracker recorded off a marathon broadcast agree with what
//! the broadcast's own board says he played?
//!
//! # The board is its own answer key
//!
//! A marathon board prints two kinds of time in the same column and they look
//! identical in a single frame: a game he has finished shows his RESULT, and
//! one he has not reached yet shows the COMPARISON — the time to beat. Over a
//! whole broadcast they are not identical at all, because the row he plays
//! changes exactly once, from the one to the other, and no other row changes
//! at all.
//!
//! So a board log answers "which games did he play, and in what time" by
//! itself. It needs no timer (the marathon total is a reading like any
//! other, and on one captured broadcast it was wrong by hours), no title (the
//! least reliable text on the pane), and no answer key read off the video by
//! hand. That is what makes it a check the project can keep: every broadcast
//! ever replayed is a test case, at no cost beyond the disk the logs sit on.
//!
//! Two refinements make the change test hold up against OCR:
//!
//! - **A value counts only once the row has settled on it** — two readings at
//!   least [`AGREE_SPREAD_MS`] apart, the same standard the tracker holds a
//!   cumulative to. Static text misread once is misread the same way again
//!   ten seconds later off nearly the same pixels, so agreement has to be
//!   spread out to mean anything.
//! - **A row that never changed was still played, if no other broadcast of
//!   the same event shows that game at that value.** A comparison time is the
//!   same number every cycle of an event; a result is not. This is what
//!   recovers a randomized board's first game, whose row appears out of
//!   nothing with the result already in it and so never changes.
//!
//! # Roster-scoped
//!
//! The key names GAMES, not row numbers: the roster the board's names fit
//! ([`crate::roster`]) says which ten games are on it, and every row is
//! assigned one of them, one-to-one. That is what lets the report say "he
//! played Zelda II and the tracker filed it under Zelda" rather than "row 10
//! was recorded", and it is what makes two rows of one broadcast recorded
//! under a single name visible as the defect it is.
//!
//! # What this does and does not prove
//!
//! It scores the tracker's PLUMBING against the board: which rows were
//! played, which were recorded, at what time, and under which of the event's
//! games. It cannot score the roster module itself, because the key folds
//! names onto games with the same [`Rosters`] the tracker uses — a roster
//! that named the wrong game would name it wrong for both. `src/roster.rs`'s
//! own tests are what hold that end.
//!
//! It is also OCR the whole way down, exactly like the thing it is checking.
//! Where it and a key read off the video by hand disagree, look at the pixels
//! before believing either: on VOD 2827296024 the two disagree over Astyanax,
//! and the hand-read one is the right one.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};

use crate::board::{game_matches, Board, BoardRow};
use crate::config::{Config, GameMode};
use crate::marathon::{
    clean_name, replay, row_cumulative_ms, Completion, Pass, AGREE, AGREE_SPREAD_MS,
};
use crate::roster::Rosters;
use crate::timeparse::format_ms_seconds;

/// How near a recorded cumulative must be to a row's own last word to be that
/// row's completion. The board prints whole seconds and one pass in a few
/// damages a digit, so a couple of seconds of slack — far inside the five
/// that separated one measured comparison time from its result.
const NEAR_MS: i64 = 2000;

/// A marathon's board runs to hours. His Ninja Gaiden board comes back with
/// ten rows too, when the pane's footer and a blank line join its six acts,
/// and on the broadcasts where the marathon pane thins to the row being
/// played it returns MORE ten-row passes than the marathon does. Its column
/// tops out at twelve minutes, which is what tells them apart without asking
/// the code under test.
const MARATHON_COLUMN_MS: i64 = 20 * 60_000;

/// How much of two boards' games must agree before they are taken to be two
/// runnings of the same event, in tenths. Only used where neither board fits
/// a roster; a numbered event says which it is itself.
const SAME_BOARD_TENTHS: usize = 6;

// ---- the derived key ----------------------------------------------------

/// One row of a board, as the key reads it over the whole broadcast.
#[derive(Debug, Clone)]
pub struct KeyRow {
    /// Position on the board, from 0.
    pub slot: usize,
    /// The name the row carried all day, as read.
    pub read: Option<String>,
    /// The event's game that name was assigned, where the board fits a
    /// roster. `None` where nothing fits, and then the row is known only by
    /// what was read off it.
    pub game: Option<String>,
    /// The first value the row settled on, and the last.
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// The last value differs from the first, or the row never changed and
    /// no other running of this event shows the game at that value.
    pub played: bool,
}

impl KeyRow {
    /// What a run off this row should be filed under.
    fn expected(&self) -> Option<&str> {
        self.game.as_deref().or(self.read.as_deref())
    }

    fn label(&self) -> &str {
        self.expected().unwrap_or("?")
    }

    /// The row was never read well enough to say anything either way.
    fn unsettled(&self) -> bool {
        self.last.is_none()
    }
}

/// One broadcast: the board's own word, and the tracker's.
#[derive(Debug)]
pub struct Broadcast {
    pub vod: String,
    /// The roster the board's names fit, by name ("#4"), where one does.
    pub event: Option<String>,
    pub rows: Vec<KeyRow>,
    pub recorded: Vec<Completion>,
    /// The replay's own description of the board at the end.
    pub summary: String,
}

/// What a run over a whole capture directory adds up to.
#[derive(Debug, Default)]
pub struct Totals {
    pub broadcasts: usize,
    /// Rows the board says were played.
    pub played: usize,
    /// Completions the tracker recorded.
    pub recorded: usize,
    /// Of those, the ones a roster named.
    pub canonical: usize,
    /// And the ones no roster name fit, filed under the reading.
    pub unmatched: usize,
    /// Played rows the tracker recorded.
    pub found: usize,
    /// Played rows it did not.
    pub missing: usize,
    /// Completions no played row accounts for.
    pub extra: usize,
    /// Completions recorded under a name that is not the key's for that row.
    pub misfiled: usize,
    /// Two completions of one broadcast under one name, which an identified
    /// event of ten distinct games can never have.
    pub duplicated: usize,
    /// Rows that never settled on a value and so say nothing either way.
    pub unsettled: usize,
    /// Every name a run was filed under, over the whole capture.
    pub names: BTreeSet<String>,
}

/// Read every broadcast under `dir`, score it, print the report, and return
/// the totals.
pub fn run(cfg: &Config, dir: &Path) -> Result<Totals> {
    let rosters = rosters_of(cfg);
    let vods = captured_vods(dir)?;
    let mut all: Vec<Broadcast> = Vec::new();
    for vod in &vods {
        let passes = passes_from_logs(dir, vod)?;
        let (recorded, summary) = replay(cfg, &passes, None);
        let (event, rows) = read_board(&passes, &rosters);
        all.push(Broadcast {
            vod: vod.clone(),
            event,
            rows,
            recorded,
            summary,
        });
    }
    // A row that never changed was played unless another running of the same
    // event shows the same game standing at the same value — which is what a
    // comparison time does and a result does not.
    for v in 0..all.len() {
        for i in 0..all[v].rows.len() {
            let Some(last) = all[v].rows[i].last else {
                continue;
            };
            let changed = all[v].rows[i].first != Some(last);
            all[v].rows[i].played = changed || !shown_elsewhere(&all, v, i, last);
        }
    }
    let mut totals = Totals {
        broadcasts: all.len(),
        ..Totals::default()
    };
    for b in &all {
        score(b, &mut totals);
    }
    println!(
        "\nTOTAL over {} broadcasts: {} of {} played rows recorded, {} recorded the board \
         does not account for, {} filed under the wrong game, {} name(s) used twice in one \
         broadcast, {} row(s) never settled",
        totals.broadcasts,
        totals.found,
        totals.played,
        totals.extra,
        totals.misfiled,
        totals.duplicated,
        totals.unsettled,
    );
    println!(
        "TOTAL completions recorded: {} ({} canonical, {} unmatched) under {} distinct names",
        totals.recorded,
        totals.canonical,
        totals.unmatched,
        totals.names.len(),
    );
    Ok(totals)
}

/// The rosters the configuration's board entry points at. A configuration
/// with none canonicalises nothing, and then the key knows rows by position
/// and by what was read off them, which is still a key.
fn rosters_of(cfg: &Config) -> Rosters {
    cfg.games
        .iter()
        .find(|g| g.mode == GameMode::Board)
        .map(|g| (*g.rosters).clone())
        .unwrap_or_default()
}

/// Every broadcast the capture directory holds, oldest first.
fn captured_vods(dir: &Path) -> Result<Vec<String>> {
    let mut vods: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading capture directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix("boards-")
                .and_then(|r| r.strip_suffix(".jsonl"))
                .map(str::to_string)
        })
        .collect();
    vods.sort();
    Ok(vods)
}

/// One line of `boards-<vod>.jsonl`: the board as the reader returned it on
/// one pane pass.
#[derive(serde::Deserialize)]
struct LoggedBoard {
    t_ms: i64,
    title: Option<String>,
    rows: Vec<LoggedRow>,
}

#[derive(serde::Deserialize)]
struct LoggedRow {
    name: Option<String>,
    cells: Vec<String>,
}

/// One line of `obs-<vod>.jsonl`, of the two fields that matter here: the
/// timer as the reader parsed it on one frame.
#[derive(serde::Deserialize)]
struct LoggedObs {
    t_ms: i64,
    parsed_ms: Option<i64>,
}

/// Every pane pass of one broadcast, with the marathon total the run loop
/// would have handed it: the last timer reading of the previous 30 s that the
/// clock could have made, which is what `app::run` feeds through
/// [`crate::sanity::Monotone`] and `app::marathon_total` returns.
///
/// `scripts/replay-arcathlon.sh` leaves both logs per broadcast. Without the
/// observation log the totals are all `None`, which is a harder test than the
/// bot faces and not a wrong one: the board's own arithmetic then has to
/// carry every completion.
pub fn passes_from_logs(dir: &Path, vod: &str) -> Result<Vec<Pass>> {
    let boards = std::fs::read_to_string(dir.join(format!("boards-{vod}.jsonl")))
        .with_context(|| format!("reading boards-{vod}.jsonl"))?;
    let obs = std::fs::read_to_string(dir.join(format!("obs-{vod}.jsonl"))).unwrap_or_default();
    let mut clock = crate::sanity::Monotone::new();
    let reads: Vec<(i64, i64)> = obs
        .lines()
        .filter_map(|l| serde_json::from_str::<LoggedObs>(l).ok())
        .filter_map(|o| o.parsed_ms.map(|v| (o.t_ms, v)))
        .filter_map(|(t, v)| clock.push(t, v).map(|v| (t, v)))
        .collect();
    let mut next = 0usize;
    let mut last: Option<(i64, i64)> = None;
    let mut out = Vec::new();
    for line in boards.lines() {
        let Ok(b) = serde_json::from_str::<LoggedBoard>(line) else {
            continue;
        };
        while next < reads.len() && reads[next].0 <= b.t_ms {
            last = Some((reads[next].1, reads[next].0));
            next += 1;
        }
        out.push(Pass {
            at_ms: b.t_ms,
            total_ms: last
                .filter(|(_, seen)| b.t_ms - seen <= 30_000)
                .map(|(v, _)| v),
            board: Board {
                title: b.title,
                subtitle: None,
                counter: None,
                rows: b
                    .rows
                    .iter()
                    .map(|r| BoardRow {
                        name: r.name.clone(),
                        cells: r.cells.clone(),
                        y: 0,
                    })
                    .collect(),
            },
        });
    }
    Ok(out)
}

/// The board's own word about a broadcast: which event it is, and for each
/// row the name it carried, the game that is, and the first and last value it
/// settled on.
///
/// Deliberately not the tracker's own machinery: it reads the whole broadcast
/// at once and takes the last word, which nothing running live can do.
pub fn read_board(passes: &[Pass], rosters: &Rosters) -> (Option<String>, Vec<KeyRow>) {
    // Only the passes that returned this marathon's whole board are read, so
    // a short pass cannot shift a row onto its neighbour.
    let mine: Vec<&Pass> = passes
        .iter()
        .filter(|p| {
            p.board
                .rows
                .iter()
                .filter_map(row_cumulative_ms)
                .max()
                .is_some_and(|m| m > MARATHON_COLUMN_MS)
        })
        .collect();
    let mut widths: HashMap<usize, usize> = HashMap::new();
    for p in mine.iter().filter(|p| p.board.rows.len() >= 6) {
        *widths.entry(p.board.rows.len()).or_insert(0) += 1;
    }
    let Some((&width, _)) = widths.iter().max_by_key(|(w, n)| (**n, **w)) else {
        return (None, Vec::new());
    };
    let all: Vec<&Pass> = mine
        .into_iter()
        .filter(|p| p.board.rows.len() == width)
        .collect();
    let consensus = |rows: &[&Pass], i: usize| -> Option<String> {
        let mut names: HashMap<String, usize> = HashMap::new();
        for p in rows {
            if let Some(n) = p.board.rows[i].name.as_deref().and_then(clean_name) {
                *names.entry(n).or_insert(0) += 1;
            }
        }
        names
            .into_iter()
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            .map(|(n, _)| n)
    };
    let carried: Vec<Option<String>> = (0..width).map(|i| consensus(&all, i)).collect();
    // And a pass counts only where a name on it is the name that row has been
    // carrying all day, so a shifted read is not laid over the board.
    let full: Vec<&Pass> = all
        .into_iter()
        .filter(|p| {
            p.board.rows.iter().enumerate().any(|(i, r)| {
                let (Some(read), Some(known)) =
                    (r.name.as_deref().and_then(clean_name), &carried[i])
                else {
                    return false;
                };
                game_matches(&read, known)
            })
        })
        .collect();
    let names: Vec<Option<String>> = (0..width).map(|i| consensus(&full, i)).collect();
    // Which event this is, and then which of its ten games each row holds —
    // the whole board at once, because no two rows of one event may be the
    // same game.
    let read: Vec<Option<&str>> = names.iter().map(Option::as_deref).collect();
    let legible: Vec<&str> = read.iter().flatten().copied().collect();
    let event = rosters.identify(&legible);
    let games: Vec<Option<String>> = rosters
        .assign(event, &read)
        .into_iter()
        .map(|g| g.map(str::to_string))
        .collect();
    // What each row settled on, in the order it settled.
    let settled: Vec<Vec<i64>> = (0..width)
        .map(|i| {
            let seen: Vec<(i64, i64)> = full
                .iter()
                .filter_map(|p| row_cumulative_ms(&p.board.rows[i]).map(|c| (p.at_ms, c)))
                .collect();
            let mut spread: HashMap<i64, (i64, i64, usize)> = HashMap::new();
            for (t, c) in &seen {
                let e = spread.entry(*c).or_insert((*t, *t, 0));
                e.1 = *t;
                e.2 += 1;
            }
            seen.iter()
                .map(|(_, c)| *c)
                .filter(|c| {
                    spread[c].2 >= AGREE as usize && spread[c].1 - spread[c].0 >= AGREE_SPREAD_MS
                })
                .collect()
        })
        .collect();
    // The column is cumulative, so what a row settled on has to be at least
    // what the row above settled on. That is what throws out a misreading
    // that settled: 51:15 read "1:15" on enough passes of one real board to
    // look settled, and it lands under the row above it.
    let (mut floor_first, mut floor_last) = (0, 0);
    let rows = (0..width)
        .map(|i| {
            let first = settled[i].iter().find(|&&v| v >= floor_first).copied();
            let last = settled[i].iter().rev().find(|&&v| v >= floor_last).copied();
            floor_first = first.unwrap_or(floor_first);
            floor_last = last.unwrap_or(floor_last);
            KeyRow {
                slot: i,
                read: names[i].clone(),
                game: games.get(i).cloned().flatten(),
                first,
                last,
                // Settled by the caller, which needs every broadcast.
                played: false,
            }
        })
        .collect();
    (event.map(|i| rosters.event_name(i).to_string()), rows)
}

/// Does another running of this event show this row's game standing at this
/// value? Then it is the comparison time and not a result.
fn shown_elsewhere(all: &[Broadcast], v: usize, i: usize, value: i64) -> bool {
    (0..all.len()).any(|w| {
        w != v
            && same_board(&all[v], &all[w])
            && all[w].rows.iter().any(|r| {
                let same_row = match (&all[v].rows[i].game, &r.game) {
                    // Named games are compared by name: a row that went
                    // unread on one broadcast shifts nothing.
                    (Some(a), Some(b)) => a == b,
                    // Nothing named them, so position is all there is.
                    _ => r.slot == i,
                };
                same_row && near(value, r.last)
            })
    })
}

/// Two broadcasts of one event. A numbered event says which it is; a
/// randomized draw names no roster and is compared row by row, where it
/// matches nothing but itself.
fn same_board(a: &Broadcast, b: &Broadcast) -> bool {
    if let (Some(x), Some(y)) = (&a.event, &b.event) {
        return x == y;
    }
    if a.event.is_some() || b.event.is_some() || a.rows.len() != b.rows.len() {
        return false;
    }
    a.rows
        .iter()
        .zip(b.rows.iter())
        .filter(|(p, q)| match (&p.read, &q.read) {
            (Some(m), Some(n)) => game_matches(m, n),
            _ => false,
        })
        .count()
        * 10
        >= a.rows.len() * SAME_BOARD_TENTHS
}

fn near(a: i64, b: Option<i64>) -> bool {
    b.is_some_and(|b| (a - b).abs() <= NEAR_MS)
}

// ---- the report ---------------------------------------------------------

/// Print one broadcast's table and add it to the totals.
fn score(b: &Broadcast, totals: &mut Totals) {
    let hit = |r: &KeyRow| b.recorded.iter().find(|c| near(c.cumulative_ms, r.last));
    let played: Vec<&KeyRow> = b.rows.iter().filter(|r| r.played).collect();
    let unsettled = b.rows.iter().filter(|r| r.unsettled()).count();
    let found = played.iter().filter(|r| hit(r).is_some()).count();
    // A completion no played row accounts for. A row that never settled is
    // not evidence either way, so a completion on one is not counted against
    // the tracker.
    let extra: Vec<&Completion> = b
        .recorded
        .iter()
        .filter(|c| {
            !played.iter().any(|r| near(c.cumulative_ms, r.last))
                && !b.rows.iter().any(|r| r.slot == c.slot && r.unsettled())
        })
        .collect();
    // Filed under a name that is not the key's for that row.
    let misfiled: Vec<(&KeyRow, &Completion)> = played
        .iter()
        .filter_map(|r| hit(r).map(|c| (*r, c)))
        .filter(|(r, c)| r.game.as_deref().is_some_and(|g| g != c.game))
        .collect();
    // Two runs of one broadcast under one name. An identified event holds ten
    // distinct games, so this is a defect on its own evidence — no answer key
    // has to be right for it to be one.
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for c in &b.recorded {
        by_name.entry(c.game.as_str()).or_default().push(c.slot + 1);
    }
    let mut duplicated: Vec<(&str, Vec<usize>)> = by_name
        .into_iter()
        .filter(|(_, slots)| slots.len() > 1)
        .collect();
    duplicated.sort();

    println!(
        "\n=== {} — event {}: {found} of {} played rows recorded, {} recorded the board does \
         not account for, {} row(s) never settled\n    {}",
        b.vod,
        b.event.as_deref().unwrap_or("(no roster fits)"),
        played.len(),
        extra.len(),
        unsettled,
        b.summary,
    );
    for r in &b.rows {
        let verdict = match (r.played, r.unsettled(), hit(r)) {
            (true, _, Some(c)) if r.game.as_deref().is_some_and(|g| g != c.game) => {
                format!("PLAYED — MISFILED as {:?} ({})", c.game, ms(c.segment_ms))
            }
            (true, _, Some(c)) => format!("PLAYED — recorded, {}", ms(c.segment_ms)),
            (true, _, None) => "PLAYED — NOT RECORDED".to_string(),
            (false, true, _) => match b.recorded.iter().find(|c| c.slot == r.slot) {
                Some(c) => format!("never settled; recorded {}", ms(c.cumulative_ms)),
                None => "never settled".to_string(),
            },
            (false, false, _) => "not played".to_string(),
        };
        println!(
            "  row {:2} {:24} {:>9} -> {:>9}  {verdict}",
            r.slot + 1,
            r.label(),
            r.first.map(ms).unwrap_or_else(|| "-".into()),
            r.last.map(ms).unwrap_or_else(|| "-".into()),
        );
    }
    for c in &extra {
        println!(
            "  !!! row {:2} {:24} {:>9}  RECORDED, and the board does not say it was played",
            c.slot + 1,
            c.game,
            ms(c.cumulative_ms),
        );
        println!(
            "BAD {} {} {} {} {}",
            b.vod,
            c.slot + 1,
            c.cumulative_ms,
            c.segment_ms,
            c.game
        );
    }
    for (name, slots) in &duplicated {
        let rows: Vec<String> = slots.iter().map(usize::to_string).collect();
        println!(
            "  !!! {name:?} recorded {} times in one broadcast, off rows {}",
            slots.len(),
            rows.join(" and "),
        );
    }
    // One machine-readable line per broadcast and per completion, for diffing
    // two runs of this against each other.
    println!(
        "SUM {} {} {found}/{} extra={} misfiled={} dup={} unsettled={unsettled} recorded={}",
        b.vod,
        b.event.as_deref().unwrap_or("-"),
        played.len(),
        extra.len(),
        misfiled.len(),
        duplicated.len(),
        b.recorded.len(),
    );
    for c in &b.recorded {
        println!(
            "REC {} {} {} {} {}",
            b.vod,
            c.slot + 1,
            c.cumulative_ms,
            c.segment_ms,
            c.game
        );
    }

    totals.played += played.len();
    totals.found += found;
    totals.missing += played.len() - found;
    totals.extra += extra.len();
    totals.misfiled += misfiled.len();
    totals.duplicated += duplicated.len();
    totals.unsettled += unsettled;
    totals.recorded += b.recorded.len();
    for c in &b.recorded {
        if c.unmatched {
            totals.unmatched += 1;
        } else {
            totals.canonical += 1;
        }
        totals.names.insert(c.game.clone());
    }
}

/// The way a board prints a time: whole seconds.
fn ms(v: i64) -> String {
    format_ms_seconds(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marathon::Pass;

    fn pass(at_ms: i64, rows: &[(&str, &str)]) -> Pass {
        Pass {
            at_ms,
            total_ms: None,
            board: Board {
                title: Some("Arcathlon".into()),
                subtitle: None,
                counter: None,
                rows: rows
                    .iter()
                    .map(|(name, cum)| BoardRow {
                        name: Some((*name).to_string()),
                        cells: vec!["1:00".into(), (*cum).to_string()],
                        y: 0,
                    })
                    .collect(),
            },
        }
    }

    fn shipped() -> Rosters {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/arcathlon-rosters.toml");
        Rosters::load(&path).expect("assets/arcathlon-rosters.toml")
    }

    /// The change test, on a board written for the purpose: nine rows hold
    /// the comparison time they opened with and one changes, so exactly one
    /// game was played and the key says which.
    #[test]
    fn the_row_that_changed_is_the_game_he_played() {
        let board = [
            ("Batman", "11:53"),
            ("Castlevania", "26:44"),
            ("Ninja Gaiden", "38:45"),
            ("Ninja Gaiden Il", "49:33"),
            ("Ninja Gaiden Ill", "1:05:01"),
            ("Super Mario Bros", "1:11:19"),
            ("Super Mario Bros 2", "1:24:06"),
            ("Super Mario Bros 3", "2:28:07"),
            ("Zelda", "3:05:22"),
            ("Zelda", "4:34:10"),
        ];
        // The board as it opened, read often enough and long enough to
        // settle, and then the last row's comparison replaced by his result.
        let mut passes: Vec<Pass> = (0..4).map(|n| pass(n * 60_000, &board)).collect();
        let mut done = board;
        done[9].1 = "5:59:54";
        passes.extend((4..8).map(|n| pass(n * 60_000, &done)));
        let (event, rows) = read_board(&passes, &shipped());
        assert_eq!(event.as_deref(), Some("#1"));
        // One game per row, so the two "Zelda" rows are the two Zeldas.
        assert_eq!(rows[8].game.as_deref(), Some("Zelda"));
        assert_eq!(rows[9].game.as_deref(), Some("Zelda II"));
        // And only the last row changed.
        assert_eq!(rows[9].first, Some(4 * 3_600_000 + 34 * 60_000 + 10_000));
        assert_eq!(rows[9].last, Some(5 * 3_600_000 + 59 * 60_000 + 54_000));
        for r in &rows[..9] {
            assert_eq!(r.first, r.last, "row {} changed", r.slot + 1);
        }
    }

    /// A reading that appeared once is not a change. The board is static text
    /// and a digit slips on one pass in a few, so a value counts only once
    /// two passes a good minute apart have agreed on it.
    #[test]
    fn a_single_misreading_is_not_a_change() {
        let board = [
            ("Batman", "11:53"),
            ("Castlevania", "26:44"),
            ("Ninja Gaiden", "38:45"),
            ("Ninja Gaiden Il", "49:33"),
            ("Ninja Gaiden Ill", "1:05:01"),
            ("Zelda", "4:34:10"),
        ];
        let mut passes: Vec<Pass> = (0..4).map(|n| pass(n * 60_000, &board)).collect();
        let mut slipped = board;
        slipped[5].1 = "4:39:10";
        passes.push(pass(4 * 60_000, &slipped));
        passes.extend((5..8).map(|n| pass(n * 60_000, &board)));
        let (_, rows) = read_board(&passes, &Rosters::default());
        assert_eq!(rows[5].first, rows[5].last, "one slip is not a change");
        assert_eq!(rows[5].last, Some(4 * 3_600_000 + 34 * 60_000 + 10_000));
        // Nothing to canonicalise against, so a row is known by its reading.
        assert_eq!(rows[5].game, None);
        assert_eq!(rows[5].read.as_deref(), Some("Zelda"));
    }
}
