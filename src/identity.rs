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
//!
//! One case defeated that rule and had to be answered differently. A board
//! whose category is the same generic "Any%" the tracked game uses, and
//! whose counter and rows are illegible, disagrees on its header alone —
//! and so does a REAL broadcast whose header came back "With)" or "Ninja
//! Gasden (NES)". Same shape, opposite meaning, so no threshold separates
//! them; counting the passes is what showed that, before a threshold was
//! changed on a guess.
//!
//! What separates them is not how many signals but WHICH game. "Klown in
//! Night Mayor World" is a game on a list this build ships; "With)" is
//! nothing at all. So a header matching a known other game contributes a
//! second signal of its own ([`Signal::NamedAnother`]), and recognising
//! the other game is positive evidence where failing to recognise this one
//! is only an absence. The lists are the Arcathlon rosters and the Big 20
//! race; a deployment with neither simply never fires that signal.

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
    /// The header names a game we have a list for, and it is not this
    /// one. Distinct from Header so a log line says which happened.
    NamedAnother,
}

impl Signal {
    pub fn label(self) -> &'static str {
        match self {
            Signal::Header => "header",
            Signal::Category => "category",
            Signal::Counter => "counter",
            Signal::Rows => "rows",
            Signal::NamedAnother => "names a known game",
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
    /// The known game the header matched, canonically spelled, when it
    /// matched one that is not the tracked game. This is what makes
    /// tracking another game possible rather than merely refusing to
    /// mis-record it: the board says "ible Dragon Il: The Revenge" and the
    /// roster says that is Double Dragon II: The Revenge.
    pub named: Option<String>,
    /// What a run of that game is filed under, when the roster that named
    /// it says (an event's `category`). None means the deployment's
    /// `game.other_category` — the fallback for a rostered game whose event
    /// does not name one.
    pub named_category: Option<String>,
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

    /// The whole pass in one line: "header, category against; counter
    /// for". Stable for a given set of signals, so the caller can log it
    /// once per distinct shape instead of once a minute.
    pub fn describe(&self) -> String {
        let side = |v: &[Signal], word| {
            (!v.is_empty()).then(|| {
                format!(
                    "{} {word}",
                    v.iter().map(|s| s.label()).collect::<Vec<_>>().join(", ")
                )
            })
        };
        match (side(&self.against, "against"), side(&self.for_it, "for")) {
            (None, None) => "nothing legible".to_string(),
            (a, f) => [a, f].into_iter().flatten().collect::<Vec<_>>().join("; "),
        }
    }
}

/// What one pass amounts to once the threshold is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing legible said anything either way.
    Silent,
    /// Spoke for the tracked game with nothing against it.
    Clear,
    /// Enough against to suspend, on its own or with the pass before it.
    Convicting,
    /// Something against, but not enough to act on.
    ///
    /// The one worth counting. A board whose category is the generic
    /// "Any%" the tracked game also uses, and whose rows and counter are
    /// illegible, disagrees on its header alone and lands here — and
    /// until this existed it left no trace in the log at all, so the one
    /// shape that can quietly record another game as this one was also
    /// the one shape nothing reported.
    Undecided,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Silent => "silent",
            Verdict::Clear => "clear",
            Verdict::Convicting => "convicting",
            Verdict::Undecided => "undecided",
        }
    }
}

/// How the passes of one session came out, for the closing line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub silent: u32,
    pub clear: u32,
    pub convicting: u32,
    pub undecided: u32,
}

impl Tally {
    pub fn describe(&self) -> String {
        format!(
            "{} clear, {} convicting, {} undecided, {} silent",
            self.clear, self.convicting, self.undecided, self.silent
        )
    }
}

/// Every game this build ships a list for, minus the one being tracked.
///
/// The Arcathlon pool and the Big 20 race are the two events this streamer
/// runs, and between them they name the games most likely to appear on a
/// pane that is not the tracked game's. Compiled in like the rosters
/// themselves; a deployment following a different streamer ships whatever
/// lists it has, or none, and then this signal never fires.
///
/// The tracked game is removed rather than matched against: Ninja Gaiden
/// is itself one of Arcathlon #1's ten, and a header reading "Ninja
/// Gaiden" must never be evidence AGAINST Ninja Gaiden.
fn bundled_rosters() -> Vec<crate::roster::Rosters> {
    [
        include_str!("../assets/arcathlon-rosters.toml"),
        include_str!("../assets/big20-roster.toml"),
    ]
    .iter()
    .filter_map(|t| crate::roster::Rosters::parse(t).ok())
    .collect()
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
    /// The game lists this build ships, kept whole rather than flattened:
    /// their own matcher is what folds a damaged reading onto a canonical
    /// name, and it is far better at it than a plain name comparison.
    rosters: Vec<crate::roster::Rosters>,
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
            rosters: bundled_rosters(),
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
    ///
    /// A header that matches a game on the KNOWN list is worth more than
    /// one that merely fails to match this game, and the difference is the
    /// whole reason the list is loaded. Counting passes showed why: a real
    /// Ninja Gaiden broadcast produces "header against" all by itself,
    /// from readings like "With)" and "Ninja Gasden (NES)", and so does a
    /// board that is genuinely another game. Same shape, opposite meaning,
    /// so no threshold can separate them — but "Klown in Night Mayor
    /// World" IS Kid Klown, and "With)" is nothing at all. Recognising the
    /// other game is positive evidence where failing to recognise this one
    /// is only an absence.
    fn header(&self, title: Option<&str>, r: &mut Reading) {
        let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) else {
            return;
        };
        if t.chars().filter(|c| c.is_alphabetic()).count() < 3 {
            return;
        }
        if board::game_matches(t, &self.game) {
            r.for_it.push(Signal::Header);
            return;
        }
        r.against.push(Signal::Header);
        if let Some((g, cat)) = self.recognise(t) {
            r.against.push(Signal::NamedAnother);
            r.named = Some(g);
            r.named_category = cat;
        }
    }

    /// Which known game this header is, canonically spelled.
    ///
    /// Through the ROSTER's matcher rather than a name comparison, because
    /// the damage is severe and structured: the pane clips leading
    /// characters, so "Double Dragon II: The Revenge" arrives as "ible
    /// Dragon Il: The Revenge" and "ble Dragon ll; The Revenge" — 21
    /// spellings in one afternoon. A plain edit-distance test folds the
    /// mild ones and misses the rest. The roster net was built for exactly
    /// this and keeps the sequel number strict, so it will not quietly
    /// turn a II into a III.
    ///
    /// The tracked game is excluded: it appears on these lists too (Ninja
    /// Gaiden is one of Arcathlon #1's ten), and its own name must never
    /// come back as "another game".
    ///
    /// Returns the name AND what a run of that game is filed under, both
    /// from the SAME roster: an event's category is a property of the event
    /// that lists the game, so it has to be read off whichever roster
    /// actually matched rather than looked up again afterwards.
    fn recognise(&self, t: &str) -> Option<(String, Option<String>)> {
        self.rosters
            .iter()
            .find_map(|r| {
                let g = r.assign(None, &[Some(t)]).first().copied().flatten()?;
                Some((g.to_string(), r.category_of(g).map(str::to_string)))
            })
            .filter(|(g, _)| !board::game_matches(g, &self.game))
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
    tally: Tally,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            ok: true,
            convicting: 0,
            threshold: 2,
            tally: Tally::default(),
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

    /// What this pass amounts to, before any hysteresis.
    pub fn verdict(&self, r: &Reading) -> Verdict {
        if self.convicts(r) {
            Verdict::Convicting
        } else if !r.against.is_empty() {
            Verdict::Undecided
        } else if r.acquits() {
            Verdict::Clear
        } else {
            Verdict::Silent
        }
    }

    /// How this session's passes have come out so far.
    pub fn tally(&self) -> Tally {
        self.tally
    }

    /// Fold in one pane pass. Returns the new verdict when it changed, so
    /// the caller logs a transition and not a heartbeat.
    pub fn observe(&mut self, r: &Reading) -> Option<bool> {
        match self.verdict(r) {
            Verdict::Silent => self.tally.silent += 1,
            Verdict::Clear => self.tally.clear += 1,
            Verdict::Convicting => self.tally.convicting += 1,
            Verdict::Undecided => self.tally.undecided += 1,
        }
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

    /// A board whose splits are numbered rather than named, read off his
    /// Double Dragon II run: rows "01" to "09" for the game's stages, an
    /// attempt counter of 1, category "JP". The rows say nothing — a
    /// number names no game, and counting them as foreign names would
    /// have them convict every board that numbers its splits, including
    /// one of his own if he ever renumbered Ninja Gaiden's acts. The other
    /// three signals are more than enough without them.
    #[test]
    fn numbered_splits_say_nothing_and_the_rest_still_convicts() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let b = board(
            Some("1"),
            &[Some("01"), Some("02"), Some("03"), Some("04"), Some("09")],
        );
        let r = f.read(Some("Double Dragon II: The Revenge"), Some("JP"), &b);
        assert!(
            !r.for_it.contains(&Signal::Rows) && !r.against.contains(&Signal::Rows),
            "numbered rows are not evidence either way: {r:?}"
        );
        assert_eq!(
            r.against,
            vec![
                Signal::Header,
                Signal::NamedAnother,
                Signal::Category,
                Signal::Counter
            ]
        );
        assert!(Identity::default().convicts(&r));
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
            vec![
                Signal::Header,
                Signal::NamedAnother,
                Signal::Counter,
                Signal::Rows
            ]
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
        assert_eq!(
            r.against,
            vec![Signal::Header, Signal::NamedAnother, Signal::Category]
        );
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

    /// The hole the tally was built to find, and its closure.
    ///
    /// Kid Klown in Night Mayor World, read off the Big 20 stream: its
    /// category is the same generic "Any%" the tracked game uses, its
    /// board shows no counter, and no row name was legible. The header was
    /// the only thing disagreeing and the category was agreeing, so the
    /// pass was undecided and a bot restarting onto that board would have
    /// recorded it as Ninja Gaiden.
    ///
    /// Knowing the game by name is what closes it. Kid Klown is on the Big
    /// 20 list, so the header is not merely failing to match Ninja Gaiden,
    /// it is matching something else — two signals, and it convicts.
    #[test]
    fn a_known_game_under_a_generic_category_now_convicts() {
        let f = Fingerprint::of(&cfg(), None);
        let r = f.read(
            Some("Klown in Night Mayor World"),
            Some("Any%"),
            &board(None, &[]),
        );
        assert_eq!(r.against, vec![Signal::Header, Signal::NamedAnother]);
        assert_eq!(r.for_it, vec![Signal::Category]);
        assert!(Identity::default().convicts(&r));
    }

    /// The residual, stated honestly: a game on NO list, whose category is
    /// the generic one and whose board shows nothing else, is still only
    /// one signal and still goes undecided. That is the correct answer —
    /// it is indistinguishable from a damaged reading of the tracked game
    /// — and it is why the tally still counts this bucket.
    #[test]
    fn an_unknown_game_under_a_generic_category_is_still_undecided() {
        let f = Fingerprint::of(&cfg(), None);
        let r = f.read(
            Some("Some Game Nobody Listed"),
            Some("Any%"),
            &board(None, &[]),
        );
        assert_eq!(r.against, vec![Signal::Header]);
        let mut id = Identity::default();
        assert_eq!(id.verdict(&r), Verdict::Undecided);
        assert_eq!(id.observe(&r), None, "the verdict does not move");
        assert_eq!(id.tally().undecided, 1, "but it is counted");
        assert_eq!(r.describe(), "header against; category for");
    }

    /// The tally is the session's own account of itself, so a week of
    /// broadcasts can be read for how often each shape came up.
    #[test]
    fn every_pass_lands_in_exactly_one_bucket() {
        let f = Fingerprint::of(&cfg(), Some(97_080));
        let mut id = Identity::default();
        let good = f.read(
            Some("Ninja Gaiden (NES)"),
            Some("Any%"),
            &board(Some("97085"), &[Some("Act 1")]),
        );
        let foreign = f.read(
            Some("Die Hard (NES)"),
            Some("Any% (Beginner)"),
            &board(Some("28971"), &[]),
        );
        // A game on no list under the generic category: one signal, which
        // is the shape that still lands undecided.
        let lone = f.read(
            Some("Some Game Nobody Listed"),
            Some("Any%"),
            &board(None, &[]),
        );

        for r in [&good, &good, &foreign, &lone, &Reading::default()] {
            id.observe(r);
        }
        let t = id.tally();
        assert_eq!((t.clear, t.convicting, t.undecided, t.silent), (2, 1, 1, 1));
        assert_eq!(t.describe(), "2 clear, 1 convicting, 1 undecided, 1 silent");
    }

    /// The fix the tally argued for, tested on the strings that argued for
    /// it — all read off real broadcasts.
    ///
    /// A damaged reading of the tracked game and a clean reading of a
    /// different one produce the SAME shape ("header against") and want
    /// opposite outcomes, so no threshold separates them. Recognising the
    /// other game does: "Klown in Night Mayor World" is a game on a list;
    /// "With)" is nothing at all.
    #[test]
    fn a_header_naming_a_known_other_game_convicts_but_damage_does_not() {
        let f = Fingerprint::of(&cfg(), None);
        let blank = board(None, &[]);

        // Real Big 20 headers, with the generic "Any%" that used to mask
        // them. Each names a game we have a list for.
        for t in [
            "Klown in Night Mayor World",
            "Kiown in Night Mayor Word",
            "ible Dragon Il: The Revenge",
            "Pac-Mania",
            "Uninvited",
            "Crisis Force",
        ] {
            let r = f.read(Some(t), Some("Any%"), &blank);
            assert!(
                r.against.contains(&Signal::NamedAnother),
                "{t} should be recognised as another game: {r:?}"
            );
            assert!(
                Identity::default().convicts(&r),
                "{t} should convict without needing the counter or rows"
            );
        }

        // Real Ninja Gaiden headers, damaged. None of these is a game on
        // any list, so none of them gains the second signal.
        for t in [
            "Ninja (NES)",
            "Ninia (NES)",
            "Ninja Gasden (NES)",
            "Ninja Garden (NES)",
            "With)",
        ] {
            let r = f.read(Some(t), Some("Any%"), &blank);
            assert!(
                !r.against.contains(&Signal::NamedAnother),
                "{t} is damage, not another game: {r:?}"
            );
            assert!(
                !Identity::default().convicts(&r),
                "{t} must not suspend a real broadcast"
            );
        }
    }

    /// The canonical name comes back, not the damaged reading. Filing runs
    /// under what OCR produced would give one game a history per spelling
    /// — Double Dragon II alone made twenty-one in an afternoon, and
    /// Excitebike's CATEGORY made three in as many minutes.
    #[test]
    fn a_recognised_board_reports_the_canonical_name() {
        let f = Fingerprint::of(&cfg(), None);
        let blank = board(None, &[]);
        for (read, want) in [
            (
                "ible Dragon Il: The Revenge",
                "Double Dragon II: The Revenge",
            ),
            (
                "ble Dragon ll; The Revenge",
                "Double Dragon II: The Revenge",
            ),
            (
                "Kiown in Night Mayor Word",
                "Kid Klown in Night Mayor World",
            ),
            ("Pac-Mana", "Pac-Mania"),
            ("Excitebike (NES)", "Excitebike"),
        ] {
            let r = f.read(Some(read), Some("Any%"), &blank);
            assert_eq!(r.named.as_deref(), Some(want), "reading {read:?}");
        }
        // A damaged reading of the TRACKED game names nothing: there is no
        // other game to file it under, and claiming one would be worse
        // than recording nothing.
        for read in ["Ninja (NES)", "With)", "Ninja Gasden (NES)"] {
            let r = f.read(Some(read), Some("Any%"), &blank);
            assert_eq!(r.named, None, "reading {read:?}");
        }
    }

    /// Ninja Gaiden is itself one of Arcathlon #1's ten games, so the list
    /// must have the tracked game removed from it or the pane would
    /// convict itself.
    #[test]
    fn the_tracked_game_is_not_on_its_own_known_list() {
        let f = Fingerprint::of(&cfg(), None);
        for t in ["Ninja Gaiden (NES)", "Ninja Gaiden"] {
            let r = f.read(Some(t), Some("Any%"), &board(None, &[]));
            assert!(r.against.is_empty(), "{t} is the tracked game: {r:?}");
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
        // A game on no list, so the header is the ONLY signal — which is
        // what isolates strict from lenient. A known game would give two
        // and both settings would convict, testing nothing.
        let r = f.read(Some("Some Game Nobody Listed"), None, &b);
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

/// Every distinct board header the bot read across 2026-09-08 and 09, and
/// what the roster makes of each. This is the corpus the folding exists
/// for: one afternoon of a twenty-game rotation produced 52 spellings of
/// seven games, because the pane clips leading characters and OCR reads
/// "II" six different ways.
///
/// The property that matters is not the hit rate. It is that NOTHING folds
/// to the WRONG game — a miss costs a recording, a mis-fold writes a false
/// one, and only the second is unrecoverable.
#[cfg(test)]
mod observed {
    use super::*;

    fn fp() -> Fingerprint {
        let mut c = crate::config::Config::for_test_with_min_final(1);
        c.game.name = "Ninja Gaiden (NES)".into();
        Fingerprint::of(&c, None)
    }

    #[test]
    fn damaged_headers_fold_onto_the_game_they_are() {
        let f = fp();
        let cases: &[(&str, &[&str])] = &[
            (
                "Double Dragon II: The Revenge",
                &[
                    "bie Dragon Il: The Revenge",
                    "ble Dragon II: The Revenge",
                    "ble Dragon Il. The Revenge",
                    "ble Dragon I: The Revenge",
                    "ble Dragon ll; The Revenge",
                    "double dragon",
                    "Dragon Il The Revenge",
                    "Dragon I! The Revenge",
                    "Dragon I) The Revenge",
                    "ibie Dragon Il: The Revenge",
                    "ible Dragon Ii: The Revenge",
                    "ible Dragon Ill The Revenge",
                    "ible Dragon It: The Revenge",
                    "ible Dragon li: The Revenge",
                    "ible Dragon ll; The Revenge",
                ],
            ),
            (
                "Kid Klown in Night Mayor World",
                &[
                    "Kfown in Night Mayor World",
                    "Kiown in Night Mayor Word",
                    "Kiown in Nig ht Mayor World",
                    "Kiown in, Night Mayor World",
                    "Klown in Night Mayor Worid",
                    "Klown Ta) Night Mayor World",
                    "Ktown in Night Mayor Word",
                    "in Night Mayor World",
                ],
            ),
            ("Pac-Mania", &["Pac-Mana", "Pac-Mania"]),
            (
                "Excitebike",
                &[
                    "Excitebike (NES",
                    "Excitebike (NES)",
                    "Excitebike Selection (NES)",
                    "Excitebske (NES)",
                ],
            ),
            ("Uninvited", &["Uninvited"]),
            ("Crisis Force", &["Crisis Force"]),
            ("Steel Legion", &["Steel Legion"]),
        ];
        for (want, reads) in cases {
            for r in *reads {
                assert_eq!(
                    f.recognise(r).map(|(g, _)| g).as_deref(),
                    Some(*want),
                    "reading {r:?}"
                );
            }
        }
    }

    #[test]
    fn nothing_that_is_not_a_game_is_taken_for_one() {
        let f = fp();
        for t in [
            // Categories, which sit one line under the header and are read
            // as the header when the game line goes illegible.
            "Selection A",
            "SelectionA",
            "Any% (Beginner)",
            // Scene artwork and a raid banner.
            "fel c",
            "romedia with raiders",
            // Too little left to identify anything.
            "down Mayor",
            // A real misread of a REAL game we do not have on a list. It
            // must not become Crisis Force just because it rhymes.
            "Life Force",
            // The tracked game, damaged. Never "another game".
            "Ninja (NES)",
            "Ninja Gasden (NES)",
            "With)",
            "Ninja Gaiden (NES)",
        ] {
            assert_eq!(f.recognise(t), None, "reading {t:?}");
        }
    }

    /// The strict sequel rule costs one fold and is worth it: "Pac-Mania
    /// 1}" carries a trailing token the roster will not discard, because
    /// discarding it is how a II becomes a III. A miss, not a mis-fold.
    #[test]
    fn a_trailing_numeral_is_not_guessed_away() {
        assert_eq!(fp().recognise("Pac-Mania 1}"), None);
    }
}
