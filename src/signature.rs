//! What a LiveSplit board *is*, read off its split rows.
//!
//! The pane's title row says what the streamer called the thing, and it is
//! worth recording, but it is the least reliable text on screen: this
//! streamer's own title comes back as "Ninja (NES" often enough to see in a
//! week of logs, and on his marathon board only the first of its two words
//! clears the OCR confidence gate. The split rows are the opposite. Every
//! board read against a hand-verified answer key matched it exactly, and
//! they carry far more: how many rows there are, whether their labels count
//! upwards or name different games, whether the last column climbs the way
//! a running total must, how large it grows, whether an attempt counter
//! sits above it.
//!
//! So identification comes from here. A [`BoardSignature`] is those
//! measurements, and [`Shape`] is the verdict they support: six rows
//! labelled "Act 1" to "Act 6" over a column reaching about eleven minutes
//! is a run board, ten rows named after different games over a column
//! reaching three hours is a marathon board, and the difference decides how
//! the broadcast should be tracked.

use crate::board::Board;
use crate::timeparse::parse_time;

/// What the row labels look like as a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Labels {
    /// One stem and a number that counts upwards: "Act 1".."Act 6",
    /// "Level 1".."Level 8". The segments of one run.
    Sequential,
    /// Different names: "Astyanax", "King Kong 2", "SMB3 (Warpless)".
    /// Different games.
    Titles,
    /// Nothing legible. On the deployed layouts the decoded crop stops
    /// short of the pane's name column, so this is the normal live case
    /// and says nothing either way.
    Absent,
}

/// The verdict, and what it implies for tracking.
///
/// The counts are **rows seen on this pane**, not the size of the board.
/// LiveSplit shows a window of the segments when there are more than fit,
/// and a row whose name and times were both unreadable is not there to be
/// counted, so a count is a floor. `partial` says when that is known to
/// matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// Segments of one repeated run: the timer resets, each attempt is a
    /// run, the rows are its acts.
    Run { acts: usize, partial: bool },
    /// A game per row, played once each: the rows complete one after
    /// another and never reset, so a completed row is a finished run of
    /// that game.
    Marathon { games: usize, partial: bool },
    /// Not enough to say. Track nothing on it.
    Unknown,
}

impl Shape {
    /// One line for a human: what this is and what to do with it.
    pub fn describe(&self) -> String {
        let count = |n: &usize, partial: &bool| {
            if *partial {
                format!("at least {n}")
            } else {
                n.to_string()
            }
        };
        match self {
            Shape::Run { acts, partial } => format!(
                "a run board of {} segments — the timer resets and each attempt is a run; \
                 track it by the timer, with these rows as its acts",
                count(acts, partial)
            ),
            Shape::Marathon { games, partial } => format!(
                "a marathon board of {} games — the rows complete one after another and \
                 nothing resets; track it by board completions, one run per game as its row fills in",
                count(games, partial)
            ),
            Shape::Unknown => "not identifiable from the rows alone".to_string(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Shape::Run { .. } => "run",
            Shape::Marathon { .. } => "marathon",
            Shape::Unknown => "unknown",
        }
    }
}

/// Measurements taken off the split rows. Every field is what was *read*,
/// not what was configured.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardSignature {
    /// Rows found above the timer.
    pub rows: usize,
    /// Rows whose name was legible.
    pub named: usize,
    /// Rows standing in for something not yet known: LiveSplit's "???" on a
    /// board whose games are drawn as it goes, or a row with no time yet.
    pub placeholders: usize,
    /// What the legible names look like as a set.
    pub labels: Labels,
    /// Time cells on a typical row: one column, or a segment beside a
    /// running total, or a signed delta beside both.
    pub columns: usize,
    /// Whether the last column never decreases down the pane, as a running
    /// total must. False means the column is a segment column, a clipped
    /// read, or a scrolling window.
    pub monotonic: bool,
    /// The largest value in the last column: an eleven-minute run board and
    /// a four-hour marathon are not the same thing.
    pub total_ms: Option<i64>,
    /// An attempt counter above the rows. LiveSplit shows one for a run
    /// repeated many times; a marathon board has nothing to count.
    pub counter: bool,
    /// Sequential labels that skip a number, which is what a scrolling
    /// board looks like: LiveSplit shows a window of the segments when
    /// there are more than fit, so "Act 3, Act 4, Act 18" is three rows of
    /// a longer list rather than a three-segment run.
    pub label_gap: bool,
}

/// The name of a row, stripped for comparison: lowercase, its trailing
/// number removed. "Act 1" and "Act 12" share the stem "act".
fn stem(name: &str) -> (String, Option<i64>) {
    let lower: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let trimmed = lower.trim_end();
    let digits: String = trimmed
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let n: Option<i64> = digits.chars().rev().collect::<String>().parse().ok();
    let head = trimmed[..trimmed.len() - digits.len()].trim_end();
    (head.split_whitespace().collect::<Vec<_>>().join(" "), n)
}

/// Whether a row's name stands in for one not yet known. His marathon board
/// prints "???" where a game has not been drawn, and OCR returns that as
/// "22?", "222", "227", "024", "077", "0?" and worse — never as letters. A
/// name with no letter in it names nothing.
fn is_placeholder(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && !n.chars().any(|c| c.is_alphabetic())
}

/// Levenshtein within `allow` edits.
fn within(a: &str, b: &str, allow: usize) -> bool {
    let n = b.chars().count();
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![0usize; n + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n] <= allow
}

/// Whether two row labels are the same words damaged differently.
///
/// Damage on these panes comes in three shapes, and each needs its own
/// allowance while none may be loose enough to merge two games:
///
/// * **Trailing.** "Act" and "Acté" for a row reading "Act 1": within one
///   edit of each other.
/// * **Leading.** Gameplay pixels left of the name column become a word, so
///   one row of six reads "AE Act 6" or "Af Act 5" while its neighbours read
///   "Act N". Seen on real frames of his own scene, and enough on its own to
///   flip the verdict, so a label whose words END the other's counts as the
///   same: "ae act" ends in "act". Two games do not become one this way,
///   because that test compares whole words — "king kong" and "donkey kong"
///   differ in their first word and stay apart.
/// * **A lost fragment.** "a" for "Act". One label being the start of the
///   other counts only when what remains is a couple of characters or a
///   number, which is what a truncated label looks like. Anything longer is
///   a different name: "Batman" is not "Batman: ROTJ", and "SMB2" is not
///   "SMB3 (Warpless)" — both pairs are on his real boards, and merging them
///   would turn a marathon into a run.
fn same_label(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    // A truncated label: what is missing is a number or a couple of letters.
    if let Some(rest) = long.strip_prefix(short) {
        let rest = rest.trim();
        if rest.chars().all(|c| c.is_ascii_digit()) || rest.chars().count() <= 2 {
            return true;
        }
    }
    // Junk prepended: the shorter label's words end the longer one's.
    let words: Vec<&str> = long.split(' ').filter(|w| !w.is_empty()).collect();
    let tail: Vec<&str> = short.split(' ').filter(|w| !w.is_empty()).collect();
    if !tail.is_empty() && words.len() > tail.len() && words[words.len() - tail.len()..] == tail[..]
    {
        return true;
    }
    within(a, b, (a.chars().count().max(b.chars().count()) / 5).max(1))
}

/// Group a set of row names by label, once damage is forgiven, and return
/// the size of each group. Six rows of one run collapse to one group of
/// six; ten games stay ten groups of one. Labels with nothing to compare (a
/// name OCR reduced to punctuation) are not counted here and are not
/// counted as names either, so the two agree.
fn label_groups(heads: &[String]) -> Vec<usize> {
    let mut groups: Vec<(&String, usize)> = Vec::new();
    for h in heads.iter().filter(|h| !h.is_empty()) {
        match groups.iter_mut().find(|(g, _)| same_label(g, h)) {
            Some((_, n)) => *n += 1,
            None => groups.push((h, 1)),
        }
    }
    groups.into_iter().map(|(_, n)| n).collect()
}

impl BoardSignature {
    /// Measure a board. `counter` says whether an attempt counter was read
    /// above the rows, which the board itself carries when the pane pass
    /// found one.
    pub fn of(board: &Board) -> Self {
        let rows = board.rows.len();
        let names: Vec<&str> = board
            .rows
            .iter()
            .filter_map(|r| r.name.as_deref())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .collect();
        let placeholders_unnamed = board
            .rows
            .iter()
            .filter(|r| r.name.is_none() && r.cells.iter().all(|c| c == "-"))
            .count();
        // A name counts only if something is left of it once the trailing
        // number is stripped. OCR returns "024" and "077" for undrawn "???"
        // slots, and a name with no word in it identifies nothing — counting
        // it as legible while it joins no label group is how a marathon with
        // one drawn game came out looking like a run.
        let stems: Vec<(String, Option<i64>)> = names
            .iter()
            .filter(|n| !is_placeholder(n))
            .map(|n| stem(n))
            .filter(|(h, _)| !h.is_empty())
            .collect();
        let heads: Vec<String> = stems.iter().map(|(h, _)| h.clone()).collect();
        let named = stems.len();
        let placeholders = names.len() - named + placeholders_unnamed;

        // Segments of one run carry one label and a number ("Act 1".."Act
        // 6"); different games carry different names. The numbers are what
        // OCR loses first, so what decides it is whether the labels agree.
        //
        // Most of them, not all: a row's name can come back as junk read off
        // the gameplay behind the pane ("im" beside "Act 1" and "Act 3" on a
        // real frame), and demanding unanimity let one such row turn his own
        // board into a marathon. Two names in three agreeing is a run board.
        //
        // Three labels are the fewest that can decide anything. Two that
        // collapse are as likely to be two games sharing a word — his own
        // boards carry "Batman" beside "Batman: ROTJ" and "SMB2" beside
        // "SMB3 (Warpless)" — as a short run board, and the cost of guessing
        // wrong is tracking a marathon as a run.
        //
        // A verdict is evidence from one frame, not a fact about the day: a
        // caller acting on it should want the same answer from several
        // passes, the way the shadow log only speaks when a board changes.
        let groups = label_groups(&heads);
        let largest = groups.iter().copied().max().unwrap_or(0);
        let labels = if named < 3 || groups.is_empty() {
            Labels::Absent
        } else if largest * 3 >= named * 2 {
            Labels::Sequential
        } else {
            Labels::Titles
        };
        // A board whose labels skip a number is a window onto a longer list.
        // Three things have to hold before saying so, because a hole in the
        // numbers is far more often a hole in the reading: every row named
        // and numbered (an unread row leaves the same gap, and the
        // highlighted row is unread as a rule), the numbers otherwise
        // ascending (a lone number read wrong is a misread), and the list
        // not starting at one (a window scrolled down from the top does not
        // begin at the first segment).
        let numbered: Vec<i64> = stems.iter().filter_map(|(_, n)| *n).collect();
        let ascending = numbered.windows(2).all(|w| w[1] > w[0]);
        let label_gap = labels == Labels::Sequential
            && numbered.len() == rows
            && ascending
            && numbered.first().is_some_and(|f| *f > 1)
            && numbered.windows(2).any(|w| w[1] > w[0] + 1);

        let columns = {
            let mut c: Vec<usize> = board
                .rows
                .iter()
                .map(|r| r.cells.iter().filter(|x| *x != "-").count())
                .filter(|n| *n > 0)
                .collect();
            c.sort_unstable();
            c.get(c.len() / 2).copied().unwrap_or(0)
        };
        // The last cell of each row that has one: the running total, when
        // the board carries one.
        let last: Vec<i64> = board
            .rows
            .iter()
            .filter_map(|r| r.cells.iter().rev().find(|c| *c != "-"))
            .filter_map(|c| parse_time(c.trim().trim_end_matches('.')))
            .collect();
        let monotonic = last.windows(2).all(|w| w[1] >= w[0]) && last.len() >= 2;
        let total_ms = last.iter().copied().max();

        BoardSignature {
            rows,
            named,
            placeholders,
            labels,
            columns,
            monotonic,
            total_ms,
            counter: board.counter.is_some(),
            label_gap,
        }
    }

    /// What this board is. Names decide it when they are legible, because
    /// segments of one run count upwards and games do not; without names
    /// the running total's size and the presence of an attempt counter are
    /// what is left to go on, and they are weaker, so an unnamed board only
    /// reaches a verdict when both agree.
    pub fn shape(&self) -> Shape {
        if self.rows < 2 {
            return Shape::Unknown;
        }
        // The row count is what this pane showed, which is the whole board
        // unless the labels say the list is scrolled. An unread *name* does
        // not make it a floor — the row was still found, by its times — so
        // only the gap counts here.
        let partial = self.label_gap;
        let run = Shape::Run {
            acts: self.rows,
            partial,
        };
        let marathon = Shape::Marathon {
            games: self.rows,
            partial,
        };
        match self.labels {
            Labels::Sequential => run,
            Labels::Titles => marathon,
            Labels::Absent => {
                // Nameless: an attempt counter means a run repeated, and a
                // total that fits inside an hour agrees with it.
                let hour = 3_600_000;
                match (self.counter, self.total_ms) {
                    (true, Some(t)) if t <= hour => run,
                    (false, Some(t)) if t > hour => marathon,
                    _ => Shape::Unknown,
                }
            }
        }
    }

    /// The signature as one line, for a log or a report.
    pub fn line(&self) -> String {
        let total = self
            .total_ms
            .map(crate::timeparse::format_ms)
            .unwrap_or_else(|| "—".into());
        format!(
            "{} rows ({} named{}{}), {} time column(s), last column {} to {}{}{}",
            self.rows,
            self.named,
            if self.placeholders > 0 {
                format!(", {} to come", self.placeholders)
            } else {
                String::new()
            },
            match self.labels {
                Labels::Sequential => ", counting up",
                Labels::Titles => ", named",
                Labels::Absent => ", unread",
            },
            self.columns,
            if self.monotonic {
                "climbing"
            } else {
                "not climbing"
            },
            total,
            if self.counter {
                ", attempt counter"
            } else {
                ""
            },
            if self.label_gap {
                ", labels skip (a scrolling window onto a longer list)"
            } else {
                ""
            },
        )
    }

    /// The signature as JSON, for a session event.
    #[allow(dead_code)] // wired into the `layout` session event in the follow-up
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "rows": self.rows,
            "named": self.named,
            "placeholders": self.placeholders,
            "labels": match self.labels {
                Labels::Sequential => "sequential",
                Labels::Titles => "titles",
                Labels::Absent => "absent",
            },
            "columns": self.columns,
            "monotonic": self.monotonic,
            "total_ms": self.total_ms,
            "counter": self.counter,
            "scrolling": self.label_gap,
            "shape": self.shape().kind(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Board, BoardRow};

    fn row(name: Option<&str>, cells: &[&str]) -> BoardRow {
        BoardRow {
            name: name.map(Into::into),
            cells: cells.iter().map(|c| c.to_string()).collect(),
            y: 0,
        }
    }

    fn board(counter: Option<&str>, rows: Vec<BoardRow>) -> Board {
        Board {
            title: Some("whatever the title says".into()),
            subtitle: None,
            counter: counter.map(Into::into),
            rows,
        }
    }

    /// His Ninja Gaiden pane: six acts counting up, a comparison column
    /// climbing to 11:39, an attempt counter. A run board.
    #[test]
    fn six_acts_over_a_climbing_column_is_a_run_board() {
        let seg = ["0:47.4", "1:53.5", "1:21.2", "2:11.8", "2:24.5", "2:56.4"];
        let cum = ["0:47.4", "2:40.9", "4:02.2", "6:14.0", "8:38.6", "11:35.1"];
        let rows = (0..6)
            .map(|i| row(Some(&format!("Act {}", i + 1)), &[seg[i], cum[i]]))
            .collect();
        let s = BoardSignature::of(&board(Some("96318"), rows));
        assert_eq!(s.rows, 6);
        assert_eq!(s.named, 6);
        assert_eq!(s.labels, Labels::Sequential);
        assert_eq!(s.columns, 2);
        assert!(s.monotonic);
        assert_eq!(s.total_ms, Some(695_100));
        assert!(s.counter && !s.label_gap);
        assert_eq!(
            s.shape(),
            Shape::Run {
                acts: 6,
                partial: false
            }
        );
        assert!(s.shape().describe().contains("timer"));
    }

    /// His marathon: ten different games, a total climbing past three
    /// hours, nothing to count attempts of. A marathon board.
    #[test]
    fn ten_named_games_over_hours_is_a_marathon_board() {
        let games = [
            ("Astyanax", "21:48", "21:48"),
            ("King Kong 2", "4:24", "26:12"),
            ("SMB3 (Warpless)", "1:03:20", "1:29:33"),
            ("Batman: ROTJ", "15:43", "1:45:16"),
            ("Kabuki Q Fighter", "13:09", "1:58:25"),
            ("Hebereke", "28:19", "2:26:44"),
            ("Batman", "12:23", "2:39:08"),
            ("SMB2", "12:04", "2:51:12"),
            ("Metal Storm", "19:15", "3:10:28"),
            ("Chip N Dale", "11:16", "3:21:45"),
        ];
        let rows = games
            .iter()
            .map(|(n, a, b)| row(Some(n), &[a, b]))
            .collect();
        let s = BoardSignature::of(&board(None, rows));
        assert_eq!(s.labels, Labels::Titles);
        assert!(s.monotonic && !s.counter);
        assert_eq!(s.total_ms, Some(12_105_000));
        assert_eq!(
            s.shape(),
            Shape::Marathon {
                games: 10,
                partial: false
            }
        );
        assert!(s.shape().describe().contains("completions"));
    }

    /// A randomized marathon partway through: three games done, one
    /// running, six slots still to be drawn showing "???" (which OCR
    /// returns as "222" and worse). The drawn names still identify it.
    #[test]
    fn undrawn_rows_do_not_confuse_the_verdict() {
        let mut rows = vec![
            row(Some("Chip 'n Dale 2"), &["20:40", "20:40"]),
            row(Some("Blaster Master"), &["33:33", "54:13"]),
            row(Some("DuckTales"), &["9:14", "1:03:28"]),
            row(Some("Arkista's Ring"), &["-", "-"]),
        ];
        for _ in 0..6 {
            rows.push(row(Some("222"), &["-", "-"]));
        }
        let s = BoardSignature::of(&board(None, rows));
        assert_eq!(s.rows, 10);
        assert_eq!(s.named, 4);
        assert_eq!(s.placeholders, 6);
        assert_eq!(s.labels, Labels::Titles);
        assert_eq!(
            s.shape(),
            Shape::Marathon {
                games: 10,
                partial: false
            }
        );
    }

    /// LiveSplit shows a window of the segments when there are more than
    /// fit, so labels that skip mean the list is longer than the board.
    /// Twenty games at his row pitch cannot all be on screen at once.
    #[test]
    fn labels_that_skip_are_a_scrolling_window() {
        let rows = [3, 4, 5, 18, 19, 20]
            .iter()
            .enumerate()
            .map(|(i, n)| {
                row(
                    Some(&format!("Level {n}")),
                    &[&format!("{}:00", i + 1), &format!("{}:00", (i + 1) * 3)],
                )
            })
            .collect();
        let s = BoardSignature::of(&board(Some("412"), rows));
        assert_eq!(s.labels, Labels::Sequential);
        assert!(s.label_gap, "a skip from Level 5 to Level 18 is a window");
        assert!(s.line().contains("scrolling"));
    }

    /// Without names — the deployed crop stops short of the name column, so
    /// this is the normal live case — the counter and the size of the total
    /// have to agree before anything is claimed.
    #[test]
    fn a_nameless_board_needs_the_counter_and_the_total_to_agree() {
        let short = || {
            (0..6)
                .map(|i| {
                    row(
                        None,
                        &[&format!("{}:00.0", i + 1), &format!("{}:00.0", (i + 1) * 2)],
                    )
                })
                .collect::<Vec<_>>()
        };
        let long = || {
            (0..10)
                .map(|i| row(None, &["30:00", &format!("{}:00:00", i + 1)]))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            BoardSignature::of(&board(Some("96318"), short())).shape(),
            Shape::Run {
                acts: 6,
                partial: false
            }
        );
        assert_eq!(
            BoardSignature::of(&board(None, long())).shape(),
            Shape::Marathon {
                games: 10,
                partial: false
            }
        );
        // A counter over an hours-long total, or neither signal: say nothing.
        assert_eq!(
            BoardSignature::of(&board(Some("96318"), long())).shape(),
            Shape::Unknown
        );
        assert_eq!(
            BoardSignature::of(&board(None, short())).shape(),
            Shape::Unknown
        );
        // Too little to measure at all.
        assert_eq!(
            BoardSignature::of(&board(None, vec![row(Some("Act 1"), &["0:47.3"])])).shape(),
            Shape::Unknown
        );
    }

    /// A clipped read is not a running total: the July 14 opening scene cut
    /// the cumulative column away and left the segment column standing in,
    /// and segment times go down as often as up.
    #[test]
    fn a_column_that_dips_is_not_a_running_total() {
        let seg = ["0:47.3", "1:53.5", "1:22.9", "2:11.7", "2:26.4", "2:57.2"];
        let rows = seg
            .iter()
            .enumerate()
            .map(|(i, s)| row(Some(&format!("Act {}", i + 1)), &[s]))
            .collect();
        let s = BoardSignature::of(&board(None, rows));
        assert!(!s.monotonic, "1:53.5 then 1:22.9 is not a total");
        assert_eq!(s.columns, 1);
        // The labels still identify it; the column only says the read was poor.
        assert_eq!(
            s.shape(),
            Shape::Run {
                acts: 6,
                partial: false
            }
        );
    }

    /// The verdicts that matter: seven real panes, read by the board reader
    /// from the words tesseract actually returned for them, classified
    /// without anyone naming the game. His two Ninja Gaiden scenes and both
    /// July 14 frames are run boards; all three marathon boards are
    /// marathons. (tests/fixtures/board/*.json — see its README.)
    #[test]
    fn the_real_panes_classify_themselves() {
        #[derive(serde::Deserialize)]
        struct FWord {
            x: u32,
            y: u32,
            w: u32,
            h: u32,
            conf: f32,
            text: String,
        }
        #[derive(serde::Deserialize)]
        struct Fixture {
            scale: u32,
            timer: (u32, u32, u32, u32),
            words: Vec<FWord>,
            letters: Vec<FWord>,
        }
        let words = |v: &[FWord]| -> Vec<crate::ocr::Word> {
            v.iter()
                .map(|w| crate::ocr::Word {
                    x: w.x,
                    y: w.y,
                    w: w.w,
                    h: w.h,
                    conf: w.conf,
                    text: w.text.clone(),
                })
                .collect()
        };
        for (name, want) in [
            ("ng-default", "run"),
            ("ng-theme", "run"),
            ("jul14-gameplay", "run"),
            ("jul14-opening", "run"),
            ("arcathlon-final", "marathon"),
            ("arcathlon-numbered", "marathon"),
            ("arcathlon-early", "marathon"),
        ] {
            let path = format!(
                "{}/tests/fixtures/board/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let f: Fixture = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
            let b =
                crate::board::read_board(&words(&f.words), &words(&f.letters), f.scale, f.timer);
            let s = BoardSignature::of(&b);
            eprintln!(
                "{name}: names {:?}\n    {} -> {}",
                b.rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
                s.line(),
                s.shape().kind()
            );
            assert_eq!(s.shape().kind(), want, "{name}: {}", s.line());
        }
    }

    /// Gameplay pixels left of the name column become a word, so one row of
    /// six comes back "AE Act 6" while its neighbours read "Act N". Seen on
    /// real 1080p frames of his own scene, where it was enough on its own to
    /// call his Ninja Gaiden board a marathon of six games.
    #[test]
    fn junk_prepended_to_one_row_does_not_change_the_board() {
        let cum = ["0:47.4", "2:40.9", "4:02.2", "6:14.0", "8:38.6", "11:35.1"];
        for damaged in ["AE Act 6", "Af Act 6", "a Act 6", "im Act 6"] {
            let rows = (0..6)
                .map(|i| {
                    let name = if i == 5 {
                        damaged.to_string()
                    } else {
                        format!("Act {}", i + 1)
                    };
                    row(Some(&name), &["1:00.0", cum[i]])
                })
                .collect();
            let s = BoardSignature::of(&board(Some("96318"), rows));
            assert_eq!(
                s.shape(),
                Shape::Run {
                    acts: 6,
                    partial: false
                },
                "{damaged}: {}",
                s.line()
            );
        }
        // The same forgiveness must not merge two games: these pairs are on
        // his real boards, and merging either would turn a marathon into a
        // run.
        assert!(!same_label("batman", "batman rotj"));
        assert!(!same_label("smb", "smb3 warpless"));
        assert!(!same_label("king kong", "donkey kong"));
        assert!(!same_label("mega man", "blaster master"));
        // …while these are one damaged label.
        assert!(same_label("act", "ae act") && same_label("act", "acté"));
        assert!(same_label("a", "act") && same_label("act", "act 12"));
    }

    /// A marathon early on: one game drawn, the rest still "???", which OCR
    /// returns as "024" and "22?". Two real frames of the August 28 event
    /// came out as run boards because a digit-only read counted as a name
    /// but joined no label group.
    #[test]
    fn digit_only_reads_of_undrawn_slots_are_not_names() {
        let mut rows = vec![
            row(Some("·"), &["-", "-"]),
            row(Some("“King Kong 2"), &["4:24", "26:12"]),
        ];
        for junk in ["22?", "22?", "22?", "22?", "22?", "024"] {
            rows.push(row(Some(junk), &["-", "-"]));
        }
        let s = BoardSignature::of(&board(None, rows));
        assert_eq!(s.named, 1, "only the drawn game is a name: {}", s.line());
        assert_eq!(s.labels, Labels::Absent);
        // One name is not enough to claim anything, and with no counter and
        // a total inside the hour the fallback cannot either. Say nothing
        // rather than call a marathon a run.
        assert_eq!(s.shape(), Shape::Unknown);
        assert!(is_placeholder("024") && is_placeholder("077") && is_placeholder("227"));
        assert!(is_placeholder("0?") && !is_placeholder("SMB2"));
    }

    /// Two labels that collapse are not a run board. His August 28 board
    /// carries "Batman" and "Batman: ROTJ"; a frame where only those two
    /// read would otherwise be one label, and one label used to mean a run.
    #[test]
    fn two_names_are_never_enough_to_call_a_board_a_run() {
        let rows = vec![
            row(Some("Batman: ROTJ"), &["15:43", "1:45:16"]),
            row(Some("Batman"), &["12:23", "2:39:08"]),
        ];
        let s = BoardSignature::of(&board(None, rows));
        assert_eq!(s.labels, Labels::Absent, "two names decide nothing");
        assert_ne!(
            s.shape(),
            Shape::Run {
                acts: 2,
                partial: false
            }
        );
    }

    /// A hole in the numbers is far more often a hole in the reading. Only
    /// a fully read, ascending list that does not begin at the first
    /// segment is a window onto a longer one.
    #[test]
    fn a_hole_in_the_reading_is_not_a_scrolling_window() {
        let named = |ns: &[&str]| -> BoardSignature {
            BoardSignature::of(&board(
                Some("96318"),
                ns.iter()
                    .enumerate()
                    .map(|(i, n)| row(Some(n), &["1:00.0", &format!("{}:00.0", i + 1)]))
                    .collect(),
            ))
        };
        // His own board with one row's number misread, and with one row's
        // name lost: neither is a window.
        assert!(!named(&["Act 1", "Act 2", "Act 3", "Act 4", "Act 8", "Act 6"]).label_gap);
        assert!(!named(&["Act 1", "Act 2", "Act 3", "Act 4", "Act 6"]).label_gap);
        assert!(!named(&["Act 1", "Act 2", "Act 3", "Act 4", "Act 5", "Act 6"]).label_gap);
        // A list scrolled down from the top: complete, ascending, and it
        // does not start at one.
        assert!(
            named(&["Level 3", "Level 4", "Level 5", "Level 18", "Level 19", "Level 20"]).label_gap
        );
    }

    #[test]
    fn stems_and_placeholders() {
        assert_eq!(stem("Act 12"), ("act".to_string(), Some(12)));
        assert_eq!(stem("Level 3"), ("level".to_string(), Some(3)));
        assert_eq!(stem("King Kong 2"), ("king kong".to_string(), Some(2)));
        assert_eq!(stem("Astyanax"), ("astyanax".to_string(), None));
        assert!(is_placeholder("???") && is_placeholder("222") && is_placeholder("- -"));
        assert!(!is_placeholder("Act 2") && !is_placeholder(""));
    }
}
