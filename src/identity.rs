//! Whether the pane is showing the tracked game's board.
//!
//! The bot used to take this on trust. It watched one channel, the channel
//! ran one game, and the stream's title was there to confirm it. Both
//! premises are gone: he streams a twenty-game rotation, and the title he
//! types ("Big 20 practice") names no game at all. On the first such
//! broadcast the bot filed another game's resets as Ninja Gaiden attempts
//! 3138 and 3139, having already read and logged the pane saying it was
//! timing something else.
//!
//! So ask the board, which is the machine's own record rather than the
//! streamer's prose. It carries four things that say what it is timing,
//! and each of them is wrong sometimes:
//!
//! * **The header.** LiveSplit prints the game above the category, and
//!   [`board::title_lines`](crate::board::title_lines) reads it. But it
//!   comes back "Ninja Garden", "Ninja Gaiden (NES", "Ninja (NES)", or
//!   below the confidence gate and not at all. A replay of one real
//!   twenty-minute Ninja Gaiden window read it wrong four times.
//! * **The category** under it, which is a field of its own: "Any%
//!   (Beginner)" is not "Any%". But it goes illegible with the header it
//!   shares a corner with.
//! * **The attempt counter.** LiveSplit counts attempts per splits file
//!   and only ever counts up, so the highest attempt a game has reached is
//!   a floor under any later reading of that game's board: a pane counting
//!   28971 for a game that reached 97080 is a different file. But a runner
//!   who starts a fresh splits file drops his own counter to 1, and that
//!   is a new season, not a new game — and a fresh replay database has no
//!   floor to compare against at all.
//! * **The row names.** "Act 1" through "Act 6" is what this game's board
//!   says down its left edge. But the name column is the first thing to go
//!   illegible, and a board read through a scene change has no names at
//!   all.
//!
//! Each failure above trips exactly one of the four, so none of them
//! convicts alone and any two convict together. That is the whole rule.
//! Silence is not evidence: a board nobody can read leaves the verdict
//! where it stood, which is what an ad break, a scene change and a webcam
//! over the pane all look like.
//!
//! Two signals is not a number picked for comfort. On the Big 20 board the
//! header, the category and the counter all disagree with Ninja Gaiden at
//! once, while every way the pane has been seen to lie disagrees in one
//! field only.

use crate::board::{self, Board};
use crate::config::Config;
use crate::signature::same_label;

/// One thing the board says about what it is timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    /// The game named in the pane's header.
    Header,
    /// The category named under it.
    Category,
    /// LiveSplit's per-splits-file attempt counter.
    Counter,
    /// The names down the left edge of the split rows.
    Rows,
}

impl Signal {
    pub fn label(self) -> &'static str {
        match self {
            Signal::Header => "header",
            Signal::Category => "category",
            Signal::Counter => "counter",
            Signal::Rows => "rows",
        }
    }
}

/// What one pane pass makes of the board, as the signals that spoke for
/// the tracked game and the signals that spoke against it. A signal that
/// could not be read appears in neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub for_it: Vec<Signal>,
    pub against: Vec<Signal>,
}

impl Reading {
    /// Whether this pass speaks for the tracked game with nothing against.
    pub fn acquits(&self) -> bool {
        !self.for_it.is_empty() && self.against.is_empty()
    }

    /// The signals against, for a log line: "header, counter".
    pub fn describe_against(&self) -> String {
        self.against
            .iter()
            .map(|s| s.label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The tracked game as its board looks: what the header should say, what
/// the rows should be called, and the highest attempt the game has already
/// reached.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    game: String,
    category: String,
    acts: Vec<String>,
    attempts: Option<i64>,
}

impl Fingerprint {
    /// Take the fingerprint from the config and the game's own history.
    /// `attempts` is the highest attempt counter this game has been seen
    /// at — `MAX(ls_attempt)` for it — and None before it has been seen at
    /// all, which mutes the counter signal rather than guessing.
    pub fn of(cfg: &Config, attempts: Option<i64>) -> Self {
        Self {
            game: cfg.game.name.clone(),
            category: cfg.game.category.clone(),
            acts: cfg.game.acts.iter().map(|a| a.name.clone()).collect(),
            attempts: attempts.filter(|&n| n > 0),
        }
    }

    /// Read one pane pass. `title` and `category` are the header's two
    /// lines as the pane reader got them, either of which is None when it
    /// was unreadable.
    pub fn read(&self, title: Option<&str>, category: Option<&str>, b: &Board) -> Reading {
        let mut r = Reading::default();
        self.header(title, &mut r);
        self.category(category, &mut r);
        self.counter(b, &mut r);
        self.rows(b, &mut r);
        r
    }

    /// The category under the header is a field of its own, and the one
    /// that carries a fresh replay through: a per-VOD database has no
    /// attempt history to compare a counter against, and a board caught
    /// at 480p often has no legible row names, which on the Big 20 stream
    /// left the header alone against the whole rule.
    ///
    /// Compared on edit distance and never on containment, because
    /// containment is what makes "Any% (Beginner)" look like "Any%" — the
    /// exact pair this has to tell apart. OCR's damage to a category is
    /// its punctuation ("Any“", "Any’:"), which survives the comparison
    /// because punctuation is stripped before it.
    fn category(&self, read: Option<&str>, r: &mut Reading) {
        let norm = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .map(|c| c.to_ascii_lowercase())
                .collect()
        };
        let (Some(seen), want) = (read.map(&norm), norm(&self.category)) else {
            return;
        };
        if seen.is_empty() || want.is_empty() {
            return;
        }
        if seen == want || board::edit_close(&seen, &want) {
            r.for_it.push(Signal::Category);
        } else {
            r.against.push(Signal::Category);
        }
    }

    /// The header names this game, or names another one. A header that
    /// reads as a category names no game and says nothing: "Any%
    /// (Beginner)" is true of a thousand boards.
    fn header(&self, title: Option<&str>, r: &mut Reading) {
        let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) else {
            return;
        };
        if t.chars().filter(|c| c.is_alphabetic()).count() < 3 {
            return;
        }
        if board::game_matches(t, &self.game) {
            r.for_it.push(Signal::Header);
        } else {
            r.against.push(Signal::Header);
        }
    }

    /// The counter sits at or above the floor this game has already
    /// reached, or far below it. "Far" is half, which no misread of a
    /// five-digit counter reaches by dropping a digit's value and every
    /// change of splits file clears by a mile.
    fn counter(&self, b: &Board, r: &mut Reading) {
        let (Some(floor), Some(seen)) = (self.attempts, counter_value(b)) else {
            return;
        };
        if seen * 2 < floor {
            r.against.push(Signal::Counter);
        } else {
            r.for_it.push(Signal::Counter);
        }
    }

    /// The rows are called what this game's acts are called. One row
    /// matching is enough to speak for it; speaking against it takes two
    /// legible names that match nothing, because a single stray word read
    /// off the gameplay behind a transparent pane is not a board.
    fn rows(&self, b: &Board, r: &mut Reading) {
        if self.acts.is_empty() {
            return;
        }
        let names: Vec<&str> = b
            .rows
            .iter()
            .filter_map(|row| row.name.as_deref())
            .map(str::trim)
            .filter(|n| n.chars().filter(|c| c.is_alphabetic()).count() >= 2)
            .collect();
        let hits = names
            .iter()
            .filter(|n| self.acts.iter().any(|a| same_label(&norm(n), &norm(a))))
            .count();
        if hits > 0 {
            r.for_it.push(Signal::Rows);
        } else if names.len() >= 2 {
            r.against.push(Signal::Rows);
        }
    }
}

/// A row name or act name reduced for comparison: lowercase, its
/// punctuation gone, its trailing number kept because "Act 1" and "Act 2"
/// are the same label to [`same_label`] either way.
fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// The attempt counter as a number. It is read as text and comes back with
/// stray punctuation, so take the digits; a reading with none is no
/// reading.
fn counter_value(b: &Board) -> Option<i64> {
    let digits: String = b
        .counter
        .as_deref()?
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|&n: &i64| n > 0)
}

/// The standing verdict on the pane, with the hysteresis that keeps one
/// bad pass from costing a broadcast.
///
/// Recording starts allowed: a bot that has read nothing yet must not sit
/// out the run it was started for. It stops after two consecutive passes
/// that convict, so a single frame caught mid-scene-change is not enough,
/// and starts again on the first pass that speaks for the game with
/// nothing against it — the same shape as the title check it replaces,
/// which was written after one garbled read took a whole broadcast with
/// it.
/// `game.require_title_match` lives on as the strict setting: it lowers
/// the bar to one signal, so a header naming another game suspends on its
/// own. Off — the default, and what the reference deployment runs — two
/// signals must agree.
#[derive(Debug, Clone)]
pub struct Identity {
    ok: bool,
    convicting: u32,
    threshold: usize,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            ok: true,
            convicting: 0,
            threshold: 2,
        }
    }
}

impl Identity {
    pub fn new(cfg: &Config) -> Self {
        Self {
            threshold: if cfg.game.require_title_match { 1 } else { 2 },
            ..Self::default()
        }
    }

    /// Whether the pane may be recorded from.
    pub fn ok(&self) -> bool {
        self.ok
    }

    /// Whether one pass is enough to convict at this strictness.
    pub fn convicts(&self, r: &Reading) -> bool {
        r.against.len() >= self.threshold
    }

    /// Fold in one pane pass. Returns the new verdict when it changed, so
    /// the caller logs a transition and not a heartbeat.
    pub fn observe(&mut self, r: &Reading) -> Option<bool> {
        if self.convicts(r) {
            self.convicting += 1;
        } else if r.acquits() {
            self.convicting = 0;
        }
        let ok = if self.convicting >= 2 {
            false
        } else if r.acquits() {
            true
        } else {
            self.ok
        };
        let changed = ok != self.ok;
        self.ok = ok;
        changed.then_some(ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::BoardRow;

    fn cfg() -> Config {
        let mut c = Config::for_test_with_min_final(1);
        c.game.name = "Ninja Gaiden (NES)".into();
        c.game.acts = (1..=6)
            .map(|i| crate::config::ActCfg {
                name: format!("Act {i}"),
                end_ms: None,
            })
            .collect();
        c
    }

    fn row(name: Option<&str>) -> BoardRow {
        BoardRow {
            name: name.map(str::to_string),
            cells: vec!["0:21.3".into()],
            y: 0,
        }
    }

    fn board(counter: Option<&str>, names: &[Option<&str>]) -> Board {
        Board {
            title: None,
            subtitle: None,
            counter: counter.map(str::to_string),
            rows: names.iter().map(|n| row(*n)).collect(),
        }
    }

    /// His Ninja Gaiden board, read cleanly: everything speaks for it.
    #[test]
    fn the_tracked_games_own_board_is_acquitted() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(Some("97081"), &[Some("Act 1"), Some("Act 2")]);
        let r = f.read(Some("Ninja Gaiden (NES)"), None, &b);
        assert_eq!(
            r.for_it,
            vec![Signal::Header, Signal::Counter, Signal::Rows]
        );
        assert!(r.against.is_empty());
        assert!(r.acquits());
        assert!(!Identity::default().convicts(&r));
    }

    /// The Big 20 board that started this: a different game, a different
    /// splits file, different rows. Three signals against, and the pass
    /// convicts on its own.
    #[test]
    fn the_die_hard_board_convicts_on_every_signal() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(
            Some("28971"),
            &[Some("Glass"), Some("Gun"), Some("Doo"), Some("Roo")],
        );
        let r = f.read(Some("Die Hard (NES)"), None, &b);
        assert_eq!(
            r.against,
            vec![Signal::Header, Signal::Counter, Signal::Rows]
        );
        assert!(Identity::default().convicts(&r));
    }

    /// A fresh splits file for a new season drops the counter to nothing
    /// while the game stays the same. The counter alone must not convict.
    #[test]
    fn a_new_splits_file_for_the_same_game_does_not_convict() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(Some("7"), &[Some("Act 1"), Some("Act 2")]);
        let r = f.read(Some("Ninja Gaiden (NES)"), None, &b);
        assert_eq!(r.against, vec![Signal::Counter]);
        assert!(!Identity::default().convicts(&r));
        assert!(!r.acquits(), "one signal against is not an acquittal");
    }

    /// Tesseract's usual damage to the header, on an otherwise good board.
    #[test]
    fn a_garbled_header_alone_does_not_convict() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(Some("97085"), &[Some("Act 3")]);
        for t in ["Ninja Garden", "Ninja Gaiden (NES", "Ninia Gaiden (NES)"] {
            let r = f.read(Some(t), None, &b);
            assert!(!Identity::default().convicts(&r), "{t} should not convict");
            assert!(r.for_it.contains(&Signal::Counter));
        }
    }

    /// The name column going illegible is not evidence of anything.
    #[test]
    fn a_board_with_no_legible_names_says_nothing_about_its_rows() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(Some("97085"), &[None, None, None]);
        let r = f.read(None, None, &b);
        assert_eq!(r.for_it, vec![Signal::Counter]);
        assert!(r.against.is_empty());
    }

    /// One stray word read off the gameplay behind a transparent pane is
    /// not a board full of foreign names.
    #[test]
    fn a_single_stray_row_name_does_not_convict_the_rows() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(None, &[Some("Holly"), None, None]);
        let r = f.read(None, None, &b);
        assert!(r.against.is_empty(), "{r:?}");
    }

    /// Nothing readable at all leaves the verdict exactly where it stood.
    #[test]
    fn an_unreadable_board_moves_nothing() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let r = f.read(None, None, &board(None, &[]));
        assert_eq!(r, Reading::default());
        let mut id = Identity::default();
        assert_eq!(id.observe(&r), None);
        assert!(id.ok());
    }

    /// Before the game has ever been seen there is no floor, so the
    /// counter says nothing rather than guessing.
    #[test]
    fn an_unseen_game_has_no_counter_floor() {
        let f = Fingerprint::of(&cfg(), None);
        let r = f.read(None, None, &board(Some("28971"), &[]));
        assert!(r.for_it.is_empty() && r.against.is_empty());
    }

    /// Two convicting passes suspend; one does not.
    #[test]
    fn suspension_takes_two_passes_and_resumption_takes_one() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let wrong = f.read(
            Some("Die Hard (NES)"),
            None,
            &board(Some("28971"), &[Some("Glass"), Some("Gun")]),
        );
        let right = f.read(
            Some("Ninja Gaiden (NES)"),
            None,
            &board(Some("97085"), &[Some("Act 1")]),
        );

        let mut id = Identity::default();
        assert_eq!(id.observe(&wrong), None, "one pass is not enough");
        assert!(id.ok());
        assert_eq!(id.observe(&wrong), Some(false), "the second suspends");
        assert!(!id.ok());
        assert_eq!(id.observe(&wrong), None, "already suspended, no news");
        assert_eq!(id.observe(&right), Some(true), "one good pass resumes");
        assert!(id.ok());
    }

    /// The category tells "Any% (Beginner)" from "Any%" while forgiving
    /// what OCR does to a percent sign. This is the signal that convicts
    /// the Big 20 board in a fresh replay database, where there is no
    /// attempt history and the row names came back blank.
    #[test]
    fn the_category_is_a_signal_of_its_own() {
        let f = Fingerprint::of(&cfg(), None);
        let blank = board(None, &[None, None, None, None]);

        let r = f.read(Some("Die Hard (NES)"), Some("Any% (Beginner)"), &blank);
        assert_eq!(r.against, vec![Signal::Header, Signal::Category]);
        assert!(
            Identity::default().convicts(&r),
            "header and category together are enough"
        );

        // Tesseract's damage to the percent sign is not a different
        // category.
        for c in ["Any%", "Any“", "Any’:", "any %"] {
            let r = f.read(Some("Ninja Gaiden (NES)"), Some(c), &blank);
            assert!(r.against.is_empty(), "{c} should read as Any%: {r:?}");
            assert!(r.for_it.contains(&Signal::Category));
        }
    }

    /// `require_title_match` lowers the bar to one signal, so a header
    /// naming another game suspends without corroboration.
    #[test]
    fn the_strict_setting_convicts_on_one_signal() {
        let mut c = cfg();
        c.game.require_title_match = true;
        let f = Fingerprint::of(&c, Some(97_080));
        let b = board(Some("97085"), &[Some("Act 1")]);
        let r = f.read(Some("Die Hard (NES)"), None, &b);
        assert_eq!(r.against, vec![Signal::Header]);
        assert!(Identity::new(&c).convicts(&r));
        assert!(!Identity::new(&cfg()).convicts(&r), "lenient needs two");
    }

    /// A convicting pass either side of an unreadable one still suspends:
    /// silence does not reset the count, or a board that flickers would
    /// never be caught.
    #[test]
    fn silence_between_two_convicting_passes_does_not_reset_the_count() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let wrong = f.read(
            Some("Die Hard (NES)"),
            None,
            &board(Some("28971"), &[Some("Glass"), Some("Gun")]),
        );
        let mut id = Identity::default();
        assert_eq!(id.observe(&wrong), None);
        assert_eq!(id.observe(&Reading::default()), None);
        assert_eq!(id.observe(&wrong), Some(false));
    }
}
