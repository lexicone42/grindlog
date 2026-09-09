//! One canonical name per game, for a marathon board.
//!
//! A completed row is filed under the name the board printed as OCR read it,
//! and `runs.game` groups by exact string. Over 38 replayed broadcasts that
//! made 119 histories out of a pool of 90 games: completions filed under a
//! spelling missing its first character or two ("nax" for Astyanax, "aws" for
//! Jaws, "Harry" for Hammerin' Harry), under a damaged numeral ("Castlevania
//! Il", "TMNT Ill"), and under the runner's own abbreviation on a randomized
//! board ("SMB2", "Kabuki Q Fighter", "Leg of Wizard" for LOTW). Each variant
//! is a separate game on the site, which is worse than any reading error the
//! tracker still makes.
//!
//! So the readings are folded onto a list of names — the [`Rosters`] file the
//! configuration points at.
//!
//! # Why rosters and not a list of ninety names
//!
//! Matching a row against all ninety is ambiguous where it matters most:
//! "Castlevania" fits Castlevania, II and III, "Duck Tales" fits Duck Tales
//! and Duck Tales 2, "Mega Man" fits six. But each of his nine numbered
//! events holds exactly ONE of each family, so inside one roster of ten there
//! is nothing to confuse and a fragment of a name is enough to place a row.
//!
//! Hence: work out which roster a board is — by how many of its legible row
//! names fit each — and then canonicalise every row against those ten. The
//! alphabet shrinks from 90 to 10 and the collisions go with it.
//!
//! # Wide inside a roster, strict outside it
//!
//! Inside a roster the net is deliberately wide: a read name that is a
//! substring of a roster name (which is what recovers a name missing its
//! first characters, and a name missing its last), one that contains a roster
//! name, one whose initials agree ("Kabuki Q Fighter", "SMB2"), or one within
//! an edit or two of some stretch of it ("stievania Il", "Litthe Mermaid").
//!
//! When no roster fits — a "Randomized Arcathlon" board draws across all nine,
//! so none will — the fallback is the whole pool, and there a trailing sequel
//! number must agree EXACTLY, after normalising the ways OCR mangles numerals
//! (`Il` for `II`, `Ill` for `III`, `l` for `1`). Without that rule the same
//! wide net maps "Ninja Gaiden Ill" onto Ninja Gaiden II, which is the exact
//! failure this module exists to prevent.
//!
//! # One game per row
//!
//! An identified event holds ten DISTINCT games, and two rows of it therefore
//! cannot be the same one. Reading each row on its own ignores that, and OCR
//! does produce the collision: on VOD 2824740806 the board's last two rows
//! are Zelda and Zelda II, the `II` was lost on most passes, both rows read
//! "Zelda", and an 84-minute Zelda II was filed under Zelda. So inside a
//! roster the rows are assigned to games ONE-TO-ONE ([`Rosters::assign`]).
//!
//! Not across the pool: a randomized draw crosses every roster, nothing says
//! its ten are distinct families, and two of its rows naming the same game is
//! a thing that legitimately happens.
//!
//! A row that matches nothing — or that lost every game it could have had —
//! keeps the name as read: nothing is dropped and nothing is guessed at. The
//! caller counts those and puts them in the session's health events, so a
//! roster that has gone stale is visible rather than silent.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// How many of a roster's games a board must name before the board is taken
/// to BE that roster. A numbered event prints all ten from the first frame
/// and reads eight or nine of them; a randomized draw of ten from ninety
/// holds about one game of any given roster, so this sits far above chance.
const ROSTER_HITS: usize = 5;

/// And by how much the best roster must beat the second. The rosters are
/// disjoint, so a runner-up scores only on fuzzy cross-matches; two clear of
/// it is a board that is not in doubt.
const ROSTER_MARGIN: usize = 2;

/// The file as it is written: `[[event]]` blocks of `name` and `games`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default, rename = "event")]
    events: Vec<RawEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEvent {
    name: String,
    games: Vec<String>,
}

/// One event's ten games, with the comparison key of each worked out once.
#[derive(Debug, Clone)]
struct Event {
    name: String,
    games: Vec<Game>,
}

#[derive(Debug, Clone)]
struct Game {
    name: String,
    key: Key,
}

/// Every event's roster, and the pool they add up to.
#[derive(Debug, Clone, Default)]
pub struct Rosters {
    events: Vec<Event>,
    /// Every game of every event, which is what a board fitting no roster is
    /// matched against.
    pool: Vec<Game>,
}

impl Rosters {
    /// Read a roster file. An empty [`Rosters`] — the default, and what a
    /// deployment that configures none has — canonicalises nothing, so every
    /// row keeps the name as read exactly as it did before this existed.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading roster file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in roster file {}", path.display()))
    }

    /// The rosters this build ships with, compiled in from `assets/`.
    ///
    /// The tracker takes its rosters from the config, because a different
    /// streamer's event is a different file. The report does not: it is
    /// naming the events in one deployment's own history, and reading a
    /// path at report time would make the site build depend on a file
    /// beside the binary. The two uses do not have to agree, and the
    /// tracker's configured file still wins wherever it is set.
    pub fn bundled() -> Result<Self> {
        Self::parse(include_str!("../assets/arcathlon-rosters.toml"))
            .context("in the bundled roster file")
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawFile = toml::from_str(text)?;
        let mut events = Vec::new();
        let mut pool: Vec<Game> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for e in raw.events {
            if e.name.trim().is_empty() {
                bail!("every [[event]] needs a name");
            }
            if e.games.is_empty() {
                bail!("event {:?} lists no games", e.name);
            }
            let mut games = Vec::new();
            for name in e.games {
                let key = Key::of(&name);
                if key.stem.is_empty() {
                    bail!("game {name:?} of event {:?} is not a name", e.name);
                }
                // The pool is what a randomized board is matched against, and
                // one name in two events would make its rows ambiguous there
                // for no reason a reader of this file could see.
                if !seen.insert(crate::board::normalise_title(&name)) {
                    bail!("game {name:?} is listed under more than one event");
                }
                let game = Game { name, key };
                pool.push(game.clone());
                games.push(game);
            }
            events.push(Event {
                name: e.name,
                games,
            });
        }
        Ok(Rosters { events, pool })
    }

    /// Is there anything here to canonicalise against?
    /// Every game named in the file, once each, in pool order. The pool is
    /// already deduplicated across events, so this is the list of games
    /// this file knows about.
    pub fn all_games(&self) -> Vec<String> {
        self.pool.iter().map(|g| g.name.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.pool.is_empty()
    }

    /// How many events, and how many games in all — for the startup log.
    pub fn size(&self) -> (usize, usize) {
        (self.events.len(), self.pool.len())
    }

    /// Which roster is this board? The one whose games most of these names
    /// fit, by [`ROSTER_HITS`] and [`ROSTER_MARGIN`], as an index into the
    /// file.
    ///
    /// Scored with the STRICT net — the one the pool fallback uses, where a
    /// trailing sequel number has to agree — because the wide net matches
    /// "Mega Man 5" to the Mega Man of another event, and a randomized board
    /// would then fit whichever roster its draw leaned towards. What is being
    /// asked here is which ten games are ON the board, and that question
    /// wants the strict answer.
    pub fn identify(&self, names: &[&str]) -> Option<usize> {
        let keys: Vec<Key> = names.iter().copied().map(Key::of).collect();
        let mut scores: Vec<(usize, usize)> = self
            .events
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut hit: HashSet<&str> = HashSet::new();
                for k in &keys {
                    if let Some(g) = pick(k, &e.games, true) {
                        hit.insert(g.name.as_str());
                    }
                }
                (hit.len(), i)
            })
            .collect();
        // Most hits first, and the earliest event of a tie, so the answer does
        // not depend on the order a sort happens to leave equal scores in.
        scores.sort_by_key(|&(hits, i)| (std::cmp::Reverse(hits), i));
        let (best, event) = *scores.first()?;
        let second = scores.get(1).map_or(0, |s| s.0);
        (best >= ROSTER_HITS && best >= second + ROSTER_MARGIN).then_some(event)
    }

    /// The name of the roster at that index, for the log ("#4").
    pub fn event_name(&self, event: usize) -> &str {
        &self.events[event].name
    }

    /// The canonical name for every row of a board at once, in row order.
    /// `None` for a row nothing fits, which then keeps the name as read.
    ///
    /// A row with no name yet — a slot the reader has never got a legible
    /// word out of — is passed as `None` and comes back as `None`. It still
    /// has to be passed, because it holds a place in the board.
    ///
    /// # Why every row together, and not one at a time
    ///
    /// Inside an identified roster the ten games are distinct, so no two rows
    /// may take the same one. That cannot be decided a row at a time: it is
    /// the rows CONTESTING a game that settles which of them gets it.
    ///
    /// They are assigned by descending confidence. The best (row, game) pair
    /// on the board — best by the same [`Rank`] a single row is matched
    /// by — takes its game; the game leaves the field; every row still open
    /// is read again against what is left, and the next-best pair goes, until
    /// nothing fits any more.
    ///
    /// Descending confidence, rather than walking the rows in order, is what
    /// keeps the answer from depending on which row is looked at first: the
    /// row that fits a game best gets it wherever on the board it sits, and a
    /// row that loses falls to its own next-best candidate. Two rows fitting
    /// one game EQUALLY well is the case that has no answer in the names —
    /// both of 2824740806's rows read exactly "Zelda" — and there the earlier
    /// row wins, because a roster lists its games in the order the board
    /// prints them.
    ///
    /// A greedy pass and not a full assignment solve: the ranks are ordinal
    /// (the shape of the match, then the sequel number, then the edits
    /// forgiven) and nothing sensible adds them up, so "the best pairing
    /// overall" is not a quantity this has. What it does have is a most
    /// confident pair, and that one is never wrong to take.
    ///
    /// Across the pool — a board no roster fits — the rows are independent,
    /// as they were: see the module docs.
    pub fn assign(&self, event: Option<usize>, reads: &[Option<&str>]) -> Vec<Option<&str>> {
        let keys: Vec<Key> = reads.iter().map(|r| Key::of(r.unwrap_or(""))).collect();
        let Some(e) = event.and_then(|i| self.events.get(i)) else {
            // Across the pool the sequel number has to agree exactly: three
            // Ninja Gaidens and six Mega Mans are told apart by nothing else.
            return keys
                .iter()
                .map(|k| pick(k, &self.pool, true).map(|g| g.name.as_str()))
                .collect();
        };
        let mut out: Vec<Option<&str>> = vec![None; keys.len()];
        let mut free = vec![true; e.games.len()];
        let mut open: Vec<usize> = (0..keys.len()).collect();
        while !open.is_empty() {
            // The most confident pair over every row still open and every
            // game still free. `min` on (rank, row, game) is best rank first,
            // then the earlier row, then the earlier game — the board's own
            // order, for a contest the names cannot settle.
            let Some((_, row, game)) = open
                .iter()
                .filter_map(|&r| {
                    // Inside a roster the net is wide and the sequel number
                    // is only a tie-break: an event holds one Castlevania, so
                    // "Castievania" is that one whether or not its numeral
                    // survived.
                    best(&keys[r], &e.games, false, &free).map(|(rank, g)| (rank, r, g))
                })
                .min()
            else {
                break;
            };
            out[row] = Some(e.games[game].name.as_str());
            free[game] = false;
            open.retain(|&r| r != row);
        }
        out
    }
}

/// The best of `games` for `read`, or None when nothing fits or two things
/// fit equally well.
///
/// `strict` gates candidates on the sequel number agreeing exactly. Without
/// it the number is still a tie-break — it is the last field of the rank —
/// which is what lets "Zelda Il" pick Zelda II out of a roster holding both
/// Zeldas while "Castievania" still finds the only Castlevania of its own.
fn pick<'a>(read: &Key, games: &'a [Game], strict: bool) -> Option<&'a Game> {
    let free = vec![true; games.len()];
    best(read, games, strict, &free).map(|(_, i)| &games[i])
}

/// [`pick`] with the games already taken by another row struck out, and with
/// the winner's rank kept: [`Rosters::assign`] compares one row's best
/// against another's, so it needs to know how good the fit was.
fn best(read: &Key, games: &[Game], strict: bool, free: &[bool]) -> Option<(Rank, usize)> {
    if read.stem.is_empty() {
        return None;
    }
    let mut ranked: Vec<(Rank, usize)> = games
        .iter()
        .enumerate()
        .filter(|(i, g)| free[*i] && (!strict || g.key.sequel == read.sequel))
        .filter_map(|(i, g)| rank(read, &g.key).map(|r| (r, i)))
        .collect();
    ranked.sort();
    let (top, game) = *ranked.first()?;
    // Two roster names fitting a reading equally well identify neither. The
    // row stays open: taking a game away from the field can leave one of them
    // standing, and then the row is no longer in any doubt.
    match ranked.get(1) {
        Some((next, _)) if *next == top => None,
        _ => Some((top, game)),
    }
}

/// How well a reading fits a name. Ordered, best first: the shape of the
/// match, then whether the sequel number agrees, then how much damage the
/// match had to forgive.
type Rank = (u8, u8, usize);

fn rank(read: &Key, name: &Key) -> Option<Rank> {
    let agree = u8::from(read.sequel != name.sequel);
    let (a, b) = (read.stem.as_str(), name.stem.as_str());
    if a == b {
        return Some((0, agree, 0));
    }
    // A substring either way: a name missing its first characters ("nax" for
    // Astyanax) or its last, and a reading that carried extra of the row with
    // it. Two characters is not a name, so it may not stand in for one.
    if a.chars().count() >= 3 && b.chars().count() >= 3 && (a.contains(b) || b.contains(a)) {
        return Some((1, agree, 0));
    }
    if initials_agree(read, name) {
        return Some((2, agree, 0));
    }
    // Damaged, and not enough of it lost to be a different name: the reading
    // is within a few edits of some stretch of the name.
    //
    // Only where the two are about as long as each other, which a whole name
    // and a damaged whole name are — "stievania" against "castlevania",
    // "Joumey to Silius" against "Journey to Silius". A short name inside a
    // much longer reading is a coincidence of letters and not a name: "Mega
    // Man" sits one edit inside "Some Game Nobody Played". A reading that
    // really is a fragment of its name is caught by the substring rule above,
    // which does not forgive damage and does not need to.
    let (la, lb) = (a.chars().count(), b.chars().count());
    if 3 * la.abs_diff(lb) > la.max(lb) {
        return None;
    }
    let d = infix_distance(a, b).min(infix_distance(b, a));
    (d * 5 <= la.min(lb)).then_some((3, agree, d))
}

/// Do two names abbreviate to the same thing?
///
/// Equal initials is the ordinary case: "Kabuki Q Fighter" and "Kabuki
/// Quantum Fighter" both make "kqf", "SMB2" and "Super Mario Bros 2" both
/// make "smb2".
///
/// The looser case needs one side to be an initialism already, and then one
/// string of initials standing inside the other in order is enough: "Leg of
/// Wizard" makes "low" and LOTW is "lotw", the two of them being the same
/// title initialised with and without its "the". Three letters minimum, and
/// no more than two apart, or the rule would marry any two acronyms.
fn initials_agree(a: &Key, b: &Key) -> bool {
    let (x, y) = (a.initials.as_str(), b.initials.as_str());
    if x.chars().count() < 3 || y.chars().count() < 3 {
        return false;
    }
    if x == y {
        return true;
    }
    if !a.acronym && !b.acronym {
        return false;
    }
    let (short, long) = if x.chars().count() <= y.chars().count() {
        (x, y)
    } else {
        (y, x)
    };
    long.chars().count() <= short.chars().count() + 2 && subsequence(short, long)
}

fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Levenshtein between `a` and the closest stretch of `b`: the start and end
/// of `b` are free, so a reading that lost its first two characters and had a
/// third damaged still finds its name.
fn infix_distance(a: &str, b: &str) -> usize {
    let bs: Vec<char> = b.chars().collect();
    // Row 0 all zeroes: a match may begin anywhere in `b`.
    let mut prev: Vec<usize> = vec![0; bs.len() + 1];
    let mut cur = vec![0usize; bs.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in bs.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    // And it may end anywhere.
    prev.into_iter().min().unwrap_or(0)
}

/// A name reduced to what two readings of it have in common.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    /// Every alphanumeric of the name, lowercased, but the sequel number.
    stem: String,
    /// A trailing "2", "II", "Ill": the thing that tells three Ninja Gaidens
    /// apart, normalised through the ways OCR writes a numeral.
    sequel: Option<u32>,
    /// The name abbreviated: an initial per word, and the whole of a word
    /// that is an abbreviation already ("SMB", "2").
    initials: String,
    /// The name is one word, so it may BE an initialism ("LOTW", "SMB2").
    acronym: bool,
}

impl Key {
    fn of(name: &str) -> Key {
        // A category in brackets is not part of the game's name: his board
        // prints "SMB3 (Warpless)" where the roster says Super Mario Bros 3.
        let words: Vec<&str> = name
            .split_whitespace()
            .filter(|w| !w.starts_with('('))
            .collect();
        let mut parts: Vec<String> = words.iter().map(|w| normal(w)).collect();
        parts.retain(|p| !p.is_empty());
        let acronym = parts.len() == 1;
        let initials: String = words
            .iter()
            .filter(|w| !normal(w).is_empty())
            .map(|w| {
                let n = normal(w);
                // A word already abbreviated keeps its letters: "SMB", "RAH",
                // "2". A word spelled out gives up its first.
                if w.chars()
                    .filter(|c| c.is_alphabetic())
                    .all(char::is_uppercase)
                {
                    n
                } else {
                    n.chars().take(1).collect()
                }
            })
            .collect();
        let sequel = split_sequel(&mut parts);
        Key {
            stem: parts.concat(),
            sequel,
            initials,
            acronym,
        }
    }
}

/// Every alphanumeric of a word, lowercased. Accents are kept: "Déjà Vu"
/// against "Déja Vu" is one edit, where dropping both would leave "djvu".
fn normal(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Take the sequel number off the end of a name, if it has one, and say what
/// it was. A name that is ONLY its number — which a row read as "Ill" is — is
/// left with no stem at all, and nothing matches a name that is not one: "Il"
/// stands inside "Guerrilla War" and a three-letter fragment has to be
/// allowed to match, so the guard belongs here.
fn split_sequel(parts: &mut Vec<String>) -> Option<u32> {
    let last = parts.last()?.clone();
    // "Super Mario Bros 2", "Mega Man 6".
    if let Some(n) = arabic(&last) {
        parts.pop();
        return Some(n);
    }
    // "Zelda II", and the same read as "Zelda Il" — tesseract writes a roman
    // numeral's I as l and 1 about as often as it gets it right.
    if let Some(n) = roman(&last) {
        parts.pop();
        return Some(n);
    }
    // Fused, the way he writes it himself: "SMB2".
    let digits: String = last
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() || digits.len() == last.len() {
        return None;
    }
    let n = digits.chars().rev().collect::<String>().parse().ok()?;
    let keep = last.len() - digits.len();
    parts.pop();
    parts.push(last[..keep].to_string());
    Some(n)
}

fn arabic(t: &str) -> Option<u32> {
    (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
        .then(|| t.parse().ok())
        .flatten()
        .filter(|n| (1..=99).contains(n))
}

/// A roman numeral up to five, read through OCR: `l` and `1` are `I`.
fn roman(t: &str) -> Option<u32> {
    let repaired: String = t
        .chars()
        .map(|c| if c == 'l' || c == '1' { 'i' } else { c })
        .collect();
    match repaired.as_str() {
        "i" => Some(1),
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        "v" => Some(5),
        "vi" => Some(6),
        "vii" => Some(7),
        "viii" => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> Rosters {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/arcathlon-rosters.toml");
        Rosters::load(&path).expect("assets/arcathlon-rosters.toml")
    }

    /// A board of one row, which is what most of these tests are about: a row
    /// on its own has nothing to contest a game with, so it gets the game it
    /// fits best whatever the rest of the board says.
    fn one<'a>(r: &'a Rosters, event: Option<usize>, name: &str) -> Option<&'a str> {
        r.assign(event, &[Some(name)]).into_iter().next().flatten()
    }

    /// A whole board at once, for the tests that are about the contest.
    fn all<'a>(r: &'a Rosters, event: Option<usize>, names: &[&str]) -> Vec<Option<&'a str>> {
        let read: Vec<Option<&str>> = names.iter().map(|n| Some(*n)).collect();
        r.assign(event, &read)
    }

    /// The file the deployment ships is nine events of ten distinct games.
    #[test]
    fn the_shipped_rosters_are_ninety_games_in_nine_events() {
        let r = shipped();
        assert_eq!(r.size(), (9, 90));
        for e in &r.events {
            assert_eq!(e.games.len(), 10, "event {} is not ten games", e.name);
        }
    }

    /// Nothing configured canonicalises nothing: a deployment without a
    /// roster file files every row under the name as read, as it always did.
    #[test]
    fn no_rosters_canonicalise_nothing() {
        let r = Rosters::default();
        assert!(r.is_empty());
        assert_eq!(r.identify(&["Astyanax", "Hebereke"]), None);
        assert_eq!(one(&r, None, "nax"), None);
    }

    #[test]
    fn a_duplicated_game_is_a_mistake_in_the_file() {
        let err = Rosters::parse(
            "[[event]]\nname = \"a\"\ngames = [\"Jaws\"]\n\
             [[event]]\nname = \"b\"\ngames = [\"jaws\"]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("more than one event"), "{err}");
        assert!(Rosters::parse("[[event]]\nname = \"a\"\ngames = []\n").is_err());
        assert!(Rosters::parse("[[event]]\nname = \"\"\ngames = [\"Jaws\"]\n").is_err());
    }

    /// The board is read as the roster it holds ten of, and a randomized draw
    /// across all nine is read as none of them.
    #[test]
    fn a_board_is_the_roster_its_rows_fit() {
        let r = shipped();
        let named = |e: Option<usize>| e.map(|i| r.event_name(i).to_string());
        let numbered = [
            "Astyanax",
            "Castlevania Il",
            "Cowboy Kid",
            "nies Il",
            "Hebereke",
            "Little Samson",
            "anic Restaurant",
            "hadowgate",
            "olstice",
            "MNT Ill",
        ];
        assert_eq!(named(r.identify(&numbered)).as_deref(), Some("#4"));
        // The randomized day of 2026-07-17, ten games over five events.
        let drawn = [
            "Chip 'n Dale 2",
            "Blaster Master",
            "DuckTales",
            "Arkista's Ring",
            "Jaws",
            "Shadowgate",
            "D.S. Indy Heat",
            "River City Ransom",
            "Harry",
            "Super C",
        ];
        assert_eq!(r.identify(&drawn), None);
        // And a board nothing has been read off yet is nothing.
        assert_eq!(r.identify(&[]), None);
        assert_eq!(r.identify(&["Astyanax"]), None);
    }

    /// Inside a roster the net is wide: a fragment of a name is enough,
    /// because the ten hold one of each family.
    #[test]
    fn a_fragment_of_a_name_finds_it_inside_its_roster() {
        let r = shipped();
        let e4 = r.identify(&[
            "Astyanax",
            "Hebereke",
            "Cowboy Kid",
            "Solstice",
            "Shadowgate",
        ]);
        assert_eq!(e4.map(|i| r.event_name(i)), Some("#4"));
        for (read, want) in [
            ("nax", "Astyanax"),
            ("bereke", "Hebereke"),
            ("hadowgate", "Shadowgate"),
            ("olstice", "Solstice"),
            ("y Kid", "Cowboy Kid"),
            ("nies Il", "Goonies II"),
            ("tie Samson", "Little Samson"),
            ("anic Restaurant", "Panic Restaurant"),
            ("stievania Il", "Castlevania II"),
            ("MNT Ill", "TMNT III"),
            ("Castievania", "Castlevania II"),
            ("TMNT II", "TMNT III"),
        ] {
            assert_eq!(one(&r, e4, read), Some(want), "{read:?}");
        }
        // And a name off some other board is still nothing.
        assert_eq!(one(&r, e4, "Jurassic Park"), None);
    }

    /// The one roster holding a whole family is told apart by the numeral,
    /// even inside it, because the numeral is the rank's last word.
    #[test]
    fn the_event_that_holds_a_whole_family_still_reads_it_right() {
        let r = shipped();
        let e1 = r.identify(&[
            "Batman",
            "Castlevania",
            "Ninja Gaiden",
            "Ninja Gaiden II",
            "Ninja Gaiden Ill",
            "Super Mario Bros",
            "SMB2",
            "SMB3 (Warpless)",
            "Zelda",
            "Zelda Il",
        ]);
        assert_eq!(e1.map(|i| r.event_name(i)), Some("#1"));
        for (read, want) in [
            ("Ninja Gaiden", "Ninja Gaiden"),
            ("Ninja Gaiden Il", "Ninja Gaiden II"),
            ("Ninja Gaiden Ill", "Ninja Gaiden III"),
            ("inja Gaiden Ill", "Ninja Gaiden III"),
            ("Zelda", "Zelda"),
            ("Zelda Il", "Zelda II"),
            ("SMB2", "Super Mario Bros 2"),
            ("SMB 2", "Super Mario Bros 2"),
            ("SMB3 (Warpless)", "Super Mario Bros 3"),
        ] {
            assert_eq!(one(&r, e1, read), Some(want), "{read:?}");
        }
    }

    /// The event's ten games are distinct, so two of its rows may not come
    /// out as the same one.
    ///
    /// VOD 2824740806, verbatim: the last two rows are Zelda and Zelda II,
    /// the numeral survived on a minority of passes, and both rows settled on
    /// "Zelda". Read a row at a time they are both Zelda, and an 84:44 Zelda
    /// II went into Zelda's history.
    #[test]
    fn two_rows_of_one_event_may_not_be_the_same_game() {
        let r = shipped();
        let board = [
            "Batman",
            "Castlevania",
            "Ninja Gaiden",
            "Ninja Gaiden I!",
            "Ninja Gaiden Ill",
            "Super Mario Bros",
            "Super Mario Bros 2",
            "Super Marlo Bros 3",
            "Zelda",
            "Zelda",
        ];
        let e1 = r.identify(&board);
        assert_eq!(e1.map(|i| r.event_name(i)), Some("#1"));
        assert_eq!(
            all(&r, e1, &board),
            [
                Some("Batman"),
                Some("Castlevania"),
                Some("Ninja Gaiden"),
                Some("Ninja Gaiden II"),
                Some("Ninja Gaiden III"),
                Some("Super Mario Bros"),
                Some("Super Mario Bros 2"),
                Some("Super Mario Bros 3"),
                Some("Zelda"),
                Some("Zelda II"),
            ]
        );
        // Read a row at a time — which is what it did before — they are one
        // game twice.
        assert_eq!(one(&r, e1, "Zelda"), Some("Zelda"));
    }

    /// The game goes to the row that fits it best, wherever on the board that
    /// row sits, and the row that loses falls to its own next-best candidate.
    /// The answer does not depend on which row is looked at first.
    #[test]
    fn the_better_reading_takes_the_game_and_the_loser_takes_the_next() {
        let r = shipped();
        let e1 = r.identify(&["Batman", "Castlevania", "Zelda", "Zelda Il", "SMB2"]);
        assert_eq!(e1.map(|i| r.event_name(i)), Some("#1"));
        // "Zelda Il" is the exact reading of Zelda II and takes it; the bare
        // "Zelda" is left with Zelda — in either order.
        assert_eq!(
            all(&r, e1, &["Zelda", "Zelda Il"]),
            [Some("Zelda"), Some("Zelda II")]
        );
        assert_eq!(
            all(&r, e1, &["Zelda Il", "Zelda"]),
            [Some("Zelda II"), Some("Zelda")]
        );
        // A real contest, with a fallback: both rows want Zelda, the whole
        // name beats the fragment, and the fragment falls to Zelda II —
        // whichever end of the board it sits at.
        assert_eq!(
            all(&r, e1, &["Zelda", "elda"]),
            [Some("Zelda"), Some("Zelda II")]
        );
        assert_eq!(
            all(&r, e1, &["elda", "Zelda"]),
            [Some("Zelda II"), Some("Zelda")]
        );
    }

    /// A row that loses its game does not then take just anything: what is
    /// left has to fit it better than everything else left, or the row keeps
    /// the name as read. "inja Gaiden Ill" is a fragment of Ninja Gaiden and
    /// of Ninja Gaiden II alike, and with Ninja Gaiden III gone it is not a
    /// reading of either.
    #[test]
    fn a_loser_whose_next_two_candidates_are_level_takes_neither() {
        let r = shipped();
        let e1 = r.identify(&["Batman", "Castlevania", "Zelda", "Zelda Il", "SMB2"]);
        assert_eq!(
            all(&r, e1, &["Ninja Gaiden III", "inja Gaiden Ill"]),
            [Some("Ninja Gaiden III"), None]
        );
        assert_eq!(
            all(&r, e1, &["inja Gaiden Ill", "Ninja Gaiden III"]),
            [None, Some("Ninja Gaiden III")]
        );
    }

    /// A row with nothing acceptable left keeps the name as read rather than
    /// taking a game that is not its own. Three rows reading "Zelda" is two
    /// Zeldas and one row the caller records under the reading and counts.
    #[test]
    fn a_row_that_loses_every_game_keeps_the_name_as_read() {
        let r = shipped();
        let e1 = r.identify(&["Batman", "Castlevania", "Ninja Gaiden", "Zelda", "SMB2"]);
        assert_eq!(
            all(&r, e1, &["Zelda", "Zelda", "Zelda"]),
            [Some("Zelda"), Some("Zelda II"), None]
        );
        // A row with no legible name at all holds its place and takes
        // nothing, so the rows after it are not shifted onto its game.
        assert_eq!(
            r.assign(e1, &[None, Some("Zelda Il"), Some("Zelda")]),
            [None, Some("Zelda II"), Some("Zelda")]
        );
    }

    /// One game per row inside a roster, and NOT across the pool: a
    /// randomized draw is ten games out of ninety with no roster to say they
    /// are distinct, and rows that name the same family there are legitimate.
    #[test]
    fn a_board_no_roster_fits_may_name_one_game_twice() {
        let r = shipped();
        assert_eq!(
            all(&r, None, &["Jaws", "aws", "Jaws"]),
            [Some("Jaws"), Some("Jaws"), Some("Jaws")]
        );
    }

    /// Across the pool the sequel number has to agree exactly, which is the
    /// whole reason the roster is worked out first.
    #[test]
    fn across_the_pool_a_sequel_number_may_not_be_guessed() {
        let r = shipped();
        assert_eq!(one(&r, None, "Ninja Gaiden Ill"), Some("Ninja Gaiden III"));
        assert_eq!(one(&r, None, "Ninja Gaiden Il"), Some("Ninja Gaiden II"));
        assert_eq!(one(&r, None, "Ninja Gaiden"), Some("Ninja Gaiden"));
        assert_eq!(one(&r, None, "TMNT Ill"), Some("TMNT III"));
        assert_eq!(one(&r, None, "Duck Tales"), Some("Duck Tales"));
        assert_eq!(one(&r, None, "DuckTales"), Some("Duck Tales"));
        assert_eq!(one(&r, None, "Duck Tales 2"), Some("Duck Tales 2"));
        assert_eq!(one(&r, None, "Mega Man 5"), Some("Mega Man 5"));
        // A fragment with no number of its own belongs to whichever game has
        // none either, and there is one of those per family.
        assert_eq!(one(&r, None, "Castievania"), Some("Castlevania"));
        // The abbreviations he writes on a randomized board.
        assert_eq!(one(&r, None, "SMB2"), Some("Super Mario Bros 2"));
        assert_eq!(one(&r, None, "Leg of Wizard"), Some("LOTW"));
        assert_eq!(
            one(&r, None, "Kabuki Q Fighter"),
            Some("Kabuki Quantum Fighter")
        );
        assert_eq!(one(&r, None, "Harry"), Some("Hammerin' Harry"));
        assert_eq!(one(&r, None, "Kong 2"), Some("King Kong 2"));
        assert_eq!(one(&r, None, "aws"), Some("Jaws"));
        assert_eq!(one(&r, None, "Déja Vu"), Some("Déjà Vu"));
    }

    /// A reading that fits nothing, or two things equally, keeps the name as
    /// read: the row is recorded and reported, never guessed at.
    #[test]
    fn a_reading_that_fits_nothing_is_left_alone() {
        let r = shipped();
        assert_eq!(one(&r, None, "Previous Segment"), None);
        assert_eq!(one(&r, None, "Sum of Best Segments"), None);
        assert_eq!(one(&r, None, "Act 1"), None);
        assert_eq!(one(&r, None, "Ill"), None);
        assert_eq!(one(&r, None, "Some Game Nobody Played"), None);
    }

    #[test]
    fn a_name_is_reduced_to_its_stem_its_number_and_its_initials() {
        let k = Key::of("Ninja Gaiden Ill");
        assert_eq!(k.stem, "ninjagaiden");
        assert_eq!(k.sequel, Some(3));
        let k = Key::of("SMB3 (Warpless)");
        assert_eq!(k.stem, "smb");
        assert_eq!(k.sequel, Some(3));
        assert_eq!(k.initials, "smb3");
        assert!(k.acronym);
        assert_eq!(Key::of("Super Mario Bros 3").initials, "smb3");
        assert_eq!(Key::of("Kabuki Q Fighter").initials, "kqf");
        assert_eq!(Key::of("Kabuki Quantum Fighter").initials, "kqf");
        assert_eq!(Key::of("Leg of Wizard").initials, "low");
        assert_eq!(Key::of("LOTW").initials, "lotw");
        // A name that is only a numeral is not a name: nothing is left of it.
        let k = Key::of("Ill");
        assert_eq!(k.stem, "");
        assert_eq!(k.sequel, Some(3));
    }

    #[test]
    fn a_stretch_of_a_name_is_found_through_the_damage() {
        assert_eq!(infix_distance("nax", "astyanax"), 0);
        assert_eq!(infix_distance("stievania", "castlevania"), 1);
        assert_eq!(infix_distance("joumeytosilius", "journeytosilius"), 2);
        assert_eq!(infix_distance("", "astyanax"), 0);
        assert_eq!(infix_distance("zzzz", "astyanax"), 4);
    }
}
