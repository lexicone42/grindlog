//! The LiveSplit pane read as a board: the title lines, the attempt counter
//! and one row per split with its name and its time cells. Built from the
//! two sparse-text OCR passes `measure_pane` already runs over the pane crop
//! (a digits-whitelisted pass and an unrestricted one), so it costs nothing
//! extra per read.
//!
//! The pane geometry (`app::pane_geometry`) only wants the cumulative column
//! of the game it was configured for; this reader wants whatever is on the
//! board — six acts of one game, or ten games of a marathon with "???" for
//! the ones not drawn yet — and says which game it is (`canonical_key`).
//! Shadow mode in this version: with `game.follow_title = "log"` the board is
//! logged and recorded as a session event, and nothing acts on it.

use crate::config::{Config, FollowTitle};
use crate::ocr;
use crate::timeparse::time_shaped;

/// A rectangle (x, y, w, h) in 1x crop pixels, as `app::R`.
type R = (u32, u32, u32, u32);

/// What one frame of the pane says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Board {
    /// The title row ("Ninja Gaiden (NES)", "Randomized Arcathlon").
    pub title: Option<String>,
    /// The line under it — the category on a one-game pane.
    pub subtitle: Option<String>,
    /// LiveSplit's attempt counter, as printed (digits only).
    pub counter: Option<String>,
    /// The split rows, top to bottom.
    pub rows: Vec<BoardRow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoardRow {
    /// The segment's name from the letters pass, or None when nothing
    /// legible stood left of the times (a highlighted row often loses it).
    pub name: Option<String>,
    /// The row's time cells left to right, ending with the segment and the
    /// cumulative column; a printed delta ("+0.3", "-21.2", the digits pass
    /// drops the sign) comes first when there is one; "-" is LiveSplit's
    /// placeholder for a time it has not got. A bare "-" in the delta
    /// column is not a cell.
    pub cells: Vec<String>,
    /// The top of the row's cells (of its name for a row without any) in
    /// 1x crop pixels.
    pub y: i64,
}

/// A word box in image pixels (the OCR pass's own scale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bx {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Bx {
    fn of(w: &ocr::Word) -> Self {
        Bx {
            x: w.x as i64,
            y: w.y as i64,
            w: w.w.max(1) as i64,
            h: w.h.max(1) as i64,
        }
    }
    fn cy(&self) -> i64 {
        self.y + self.h / 2
    }
    fn right(&self) -> i64 {
        self.x + self.w
    }
    fn bottom(&self) -> i64 {
        self.y + self.h
    }
}

/// A time cell candidate from either pass.
#[derive(Debug, Clone)]
struct Cell {
    b: Bx,
    text: String,
    /// A time with minutes ("1:53.8", "1:03:20"), as opposed to a delta
    /// ("0.3") which LiveSplit prints without a colon.
    colon: bool,
    /// From the digits-whitelisted pass (its digits are the more reliable
    /// of the two; the letters pass has the sign).
    digits_pass: bool,
}

/// One row while it is being assembled.
#[derive(Debug, Clone)]
struct Row {
    cells: Vec<Cell>,
    names: Vec<(Bx, String)>,
    cy: i64,
    top: i64,
    h: i64,
}

impl Row {
    fn from_cell(c: Cell) -> Self {
        Row {
            cy: c.b.cy(),
            top: c.b.y,
            h: c.b.h,
            cells: vec![c],
            names: Vec::new(),
        }
    }
    fn from_names(words: Vec<(Bx, String)>) -> Self {
        let b = words[0].0;
        Row {
            cy: b.cy(),
            top: words.iter().map(|(b, _)| b.y).min().unwrap_or(b.y),
            h: b.h,
            cells: Vec::new(),
            names: words,
        }
    }
}

/// The dashes LiveSplit (and tesseract) print for a missing time: ASCII
/// hyphen, em and en dash, the true minus sign.
const DASHES: [char; 4] = ['-', '\u{2014}', '\u{2013}', '\u{2212}'];

fn dash_only(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| DASHES.contains(&c))
}

/// A word that is a time cell: after a leading sign (or whatever tesseract
/// made of one — a quote, a stray glyph) the rest is time-shaped. Returns
/// the cell text, with the sign normalised to "+"/"-" when one was read,
/// and whether it has a colon. Trailing punctuation the row highlight adds
/// ("8:38.6:") is trimmed.
fn time_cell(raw: &str) -> Option<(String, bool)> {
    let t = raw.trim();
    let body = t.trim_start_matches(|c: char| !c.is_ascii_digit());
    let prefix = &t[..t.len() - body.len()];
    let body = body.trim_end_matches(['.', ':']);
    if body.is_empty() || !time_shaped(body) {
        return None;
    }
    let sign = if prefix.contains('+') {
        "+"
    } else if prefix.chars().any(|c| DASHES.contains(&c)) {
        "-"
    } else {
        ""
    };
    Some((format!("{sign}{body}"), body.contains(':')))
}

/// A word that can be part of a segment name: it has something
/// alphanumeric in it (bars, quotes and asterisks are border and artwork
/// junk; "???", LiveSplit's name for a game not drawn yet, is allowed), and
/// it is not a number with a point or colon ("733.7" is a delta whose sign
/// tesseract turned into a digit; the "2" of "Gremlins 2" stays).
fn name_word(t: &str) -> bool {
    t.chars().any(|c| c.is_alphanumeric() || c == '?')
        && !(t.contains(['.', ':'])
            && t.chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == ':'))
}

fn median(v: &mut [i64]) -> Option<i64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// Group boxes into lines by vertical centre, each line sorted left to right.
pub(crate) fn into_lines(mut boxes: Vec<(R, String)>) -> Vec<Vec<(R, String)>> {
    let cy = |r: R| r.1 as i64 + r.3 as i64 / 2;
    boxes.sort_by_key(|(r, _)| cy(*r));
    let mut lines: Vec<Vec<(R, String)>> = Vec::new();
    for b in boxes {
        match lines.last_mut() {
            Some(line) if (cy(line[0].0) - cy(b.0)).abs() <= (b.0 .3 as i64 / 2).max(6) => {
                line.push(b)
            }
            _ => lines.push(vec![b]),
        }
    }
    for line in &mut lines {
        line.sort_by_key(|(r, _)| r.0);
    }
    lines
}

/// The title lines above the split rows, from the letters pass: the title
/// is the line with the most letters (at least four) — the topmost line is
/// not necessarily it, themed layouts have artwork above the pane that OCR
/// turns into stray syllables — and the subtitle is whatever sits directly
/// under it. Words are taken at confidence 30 and up, within the timer's
/// horizontal band, no taller than twice the timer crop, with something
/// alphanumeric in them and not digits alone (that is the attempt counter).
/// `splits_top` is the top of the split rows in 1x pixels; nothing at or
/// below it is a title line. Shared by `app::pane_readings` and
/// `read_board` so the two agree on what the pane is called.
pub(crate) fn title_lines(
    letters: &[ocr::Word],
    scale: u32,
    timer: R,
    splits_top: i64,
) -> (Option<String>, Option<String>) {
    let sc = scale.max(1);
    let band_x0 = timer.0.saturating_sub(timer.2) as i64;
    let band_x1 = (timer.0 + timer.2 * 2) as i64;
    let above: Vec<(R, String)> = letters
        .iter()
        .filter(|w| w.conf >= 30.0 && !w.text.trim().is_empty())
        .map(|w| {
            (
                (w.x / sc, w.y / sc, w.w.max(1) / sc, w.h.max(1) / sc),
                w.text.trim().to_string(),
            )
        })
        .filter(|(r, text)| {
            (r.1 + r.3) as i64 <= splits_top + 4
                && (r.0 + r.2) as i64 > band_x0
                && (r.0 as i64) < band_x1
                && r.3 < timer.3 * 2
                && text.chars().any(char::is_alphanumeric)
                && !text
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
        })
        .collect();
    let lines: Vec<String> = into_lines(above)
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| s.chars().any(|c| c.is_alphabetic()))
        .collect();
    let letters_in = |s: &str| s.chars().filter(|c| c.is_alphabetic()).count();
    let title_idx = lines
        .iter()
        .enumerate()
        .filter(|(_, s)| letters_in(s) >= 4)
        .max_by_key(|(_, s)| letters_in(s))
        .map(|(i, _)| i);
    (
        title_idx.map(|i| lines[i].clone()),
        title_idx.and_then(|i| lines.get(i + 1).cloned()),
    )
}

/// Read the board from the two OCR passes over the pane crop (`words`: the
/// digits-whitelisted pass, `letters`: the unrestricted one, both at
/// `scale` times the crop's pixels; `timer` is the timer's rectangle in
/// crop pixels).
///
/// Rows are found from their time cells: time-shaped words above the timer
/// from either pass, grouped by vertical centre. Boxes over 1.6x the median
/// cell height are dropped first (the timer's own digits reaching above its
/// crop, a highlighted row's cells merged into one). Two rules add rows
/// that have no time to show: a dash the letters pass read right-aligned
/// with a column the other rows fill with times is a placeholder cell (the
/// running row of a marathon board prints "-" in both columns); and once
/// two rows give a pitch, a line of name words sitting one pitch under the
/// row above it, left of the columns, is a row too ("???" for a game not
/// drawn yet, a highlighted row whose times went unread). A row's name is
/// the letters-pass words on its line left of its first cell.
pub fn read_board(words: &[ocr::Word], letters: &[ocr::Word], scale: u32, timer: R) -> Board {
    let sc = scale.max(1) as i64;
    let tm = Bx {
        x: timer.0 as i64 * sc,
        y: timer.1 as i64 * sc,
        w: timer.2 as i64 * sc,
        h: timer.3 as i64 * sc,
    };
    let above = |b: &Bx| b.bottom() <= tm.y + 4 * sc && b.h < tm.h;

    // Time cells from both passes.
    let mut cells: Vec<Cell> = Vec::new();
    for (pass, digits_pass) in [(words, true), (letters, false)] {
        for w in pass {
            let b = Bx::of(w);
            if !above(&b) {
                continue;
            }
            if let Some((text, colon)) = time_cell(&w.text) {
                cells.push(Cell {
                    b,
                    text,
                    colon,
                    digits_pass,
                });
            }
        }
    }
    let mut heights: Vec<i64> = cells.iter().map(|c| c.b.h).collect();
    let med_h = median(&mut heights);
    if let Some(m) = med_h {
        cells.retain(|c| c.b.h * 10 <= m * 16);
    }

    // Rows by vertical centre; within a row, one cell per box (both passes
    // see the same box): the signed reading wins, then the digits pass.
    cells.sort_by_key(|c| c.b.cy());
    let mut rows: Vec<Row> = Vec::new();
    for c in cells {
        match rows.last_mut() {
            Some(r) if (r.cy - c.b.cy()).abs() <= (c.b.h.max(r.h) / 2).max(3 * sc) => {
                r.cells.push(c)
            }
            _ => rows.push(Row::from_cell(c)),
        }
    }
    for r in &mut rows {
        r.cells.sort_by_key(|c| c.b.x);
        let mut kept: Vec<Cell> = Vec::new();
        for c in r.cells.drain(..) {
            let dup = kept.last().is_some_and(|k| {
                let overlap = k.b.right().min(c.b.right()) - k.b.x.max(c.b.x);
                overlap * 2 >= k.b.w.min(c.b.w)
            });
            if dup {
                let k = kept.last_mut().unwrap();
                let score = |c: &Cell| {
                    u8::from(c.text.starts_with(['+', '-'])) * 2 + u8::from(c.digits_pass)
                };
                if score(&c) > score(k) {
                    *k = c;
                }
            } else {
                kept.push(c);
            }
        }
        r.cells = kept;
        r.top = r.cells.iter().map(|c| c.b.y).min().unwrap_or(r.top);
    }

    // The row pitch, once two rows give one.
    let mut gaps: Vec<i64> = rows.windows(2).map(|p| p[1].cy - p[0].cy).collect();
    let pitch = median(&mut gaps).filter(|&p| p > 0);
    let near = pitch.map(|p| p / 2).or(med_h).unwrap_or(6 * sc);

    // The right edges of the columns that hold real times (a colon), for
    // placing dashes: LiveSplit right-aligns its columns, so a column is an
    // edge at least two cells share; a lone edge is a cell that merged with
    // something to its right.
    let mut all_edges: Vec<i64> = rows
        .iter()
        .flat_map(|r| r.cells.iter())
        .filter(|c| c.colon)
        .map(|c| c.b.right())
        .collect();
    all_edges.sort_unstable();
    let mut edges: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < all_edges.len() {
        let mut j = i + 1;
        while j < all_edges.len() && all_edges[j] - all_edges[i] <= 8 * sc {
            j += 1;
        }
        if j - i >= 2 {
            edges.push(all_edges[(i + j) / 2]);
        }
        i = j;
    }
    let mut dashes: Vec<Bx> = letters
        .iter()
        .filter(|w| w.conf >= 30.0 && dash_only(w.text.trim()))
        .map(Bx::of)
        .filter(|b| above(b) && edges.iter().any(|&e| (b.right() - e).abs() <= 8 * sc))
        .collect();
    dashes.sort_by_key(|b| (b.cy(), b.x));
    for d in dashes {
        let cell = Cell {
            b: d,
            text: "-".to_string(),
            colon: false,
            digits_pass: false,
        };
        match rows
            .iter_mut()
            .min_by_key(|r| (r.cy - d.cy()).abs())
            .filter(|r| (r.cy - d.cy()).abs() <= near)
        {
            Some(r) => {
                r.cells.push(cell);
                r.cells.sort_by_key(|c| c.b.x);
            }
            None => {
                // A row of placeholders alone, if it sits where a row can:
                // within a row and a half of the rows read.
                let span = pitch.map(|p| p * 3 / 2).unwrap_or(near * 3);
                let first = rows.first().map(|r| r.cy).unwrap_or(d.cy());
                let last = rows.last().map(|r| r.cy).unwrap_or(d.cy());
                if d.cy() + span >= first && d.cy() <= last + span {
                    let mut r = Row::from_cell(cell);
                    r.h = near; // a dash is a few pixels tall; give the row a row's height
                    rows.push(r);
                    rows.sort_by_key(|r| r.cy);
                }
            }
        }
    }

    // Name words: confident, alphanumeric, not a cell, no taller than twice
    // a cell (a highlight bar read as one word spans the row), and not a
    // sliver on the crop's left edge (a pane border cut by the crop reads
    // as a thin "i" or "|" on every row).
    let mut name_words: Vec<(Bx, String)> = letters
        .iter()
        .filter(|w| w.conf >= 30.0)
        .map(|w| (Bx::of(w), w.text.trim().to_string()))
        .filter(|(b, t)| {
            above(b)
                && name_word(t)
                && !dash_only(t)
                && time_cell(t).is_none()
                && med_h.is_none_or(|m| b.h <= 2 * m && !(b.x == 0 && b.w * 2 < m))
        })
        .collect();
    name_words.sort_by_key(|(b, _)| (b.cy(), b.x));
    let cells_left = rows
        .iter()
        .flat_map(|r| r.cells.iter())
        .map(|c| c.b.x)
        .min();
    // Words on a row's line go to it; the rest form lines of their own.
    let mut loose: Vec<(Bx, String)> = Vec::new();
    for (b, t) in name_words {
        match rows
            .iter_mut()
            .min_by_key(|r| (r.cy - b.cy()).abs())
            .filter(|r| (r.cy - b.cy()).abs() <= near)
        {
            Some(r) => r.names.push((b, t)),
            None => loose.push((b, t)),
        }
    }
    if let (Some(p), Some(left)) = (pitch, cells_left) {
        let mut lines: Vec<Vec<(Bx, String)>> = Vec::new();
        for (b, t) in loose {
            match lines.last_mut() {
                Some(l) if (l[0].0.cy() - b.cy()).abs() <= (b.h.max(l[0].0.h) / 2).max(3 * sc) => {
                    l.push((b, t))
                }
                _ => lines.push(vec![(b, t)]),
            }
        }
        for line in lines {
            let right = line.iter().map(|(b, _)| b.right()).max().unwrap_or(0);
            if right > left + 4 * sc {
                continue; // reaches into the columns: not a name line
            }
            let cy = line[0].0.cy();
            // One pitch (or a whole number of them) under the nearest row
            // above, within a third of a pitch. Walking down, each accepted
            // row anchors the next, so a pitch off by a pixel does not drift.
            let Some(anchor) = rows.iter().filter(|r| r.cy < cy).map(|r| r.cy).max() else {
                continue;
            };
            let k = ((cy - anchor) as f64 / p as f64).round().max(1.0) as i64;
            if (cy - (anchor + k * p)).abs() <= p / 3 {
                rows.push(Row::from_names(line));
                rows.sort_by_key(|r| r.cy);
            }
        }
    }

    // Names: the words left of the row's first cell (of the columns, for a
    // row without cells), left to right.
    let out_rows: Vec<BoardRow> = rows
        .iter()
        .map(|r| {
            let limit = r
                .cells
                .first()
                .map(|c| c.b.x)
                .or(cells_left)
                .unwrap_or(i64::MAX);
            let mut names: Vec<&(Bx, String)> = r
                .names
                .iter()
                .filter(|(b, _)| b.right() <= limit + 2 * sc)
                .collect();
            names.sort_by_key(|(b, _)| b.x);
            let name: Vec<&str> = names.iter().map(|(_, t)| t.as_str()).collect();
            BoardRow {
                name: (!name.is_empty()).then(|| name.join(" ")),
                cells: r.cells.iter().map(|c| c.text.clone()).collect(),
                y: r.top / sc,
            }
        })
        .collect();

    // Above the rows: the title lines (bounded by the first row's top — the
    // same boundary `measure_pane` hands `pane_readings`, so the two read
    // one title; its centre would let the row's own words in) and the
    // attempt counter (a bare integer of three digits or more, the lowest).
    let rows_top = rows.first().map(|r| r.top).unwrap_or(tm.y);
    let (title, subtitle) = title_lines(letters, scale, timer, rows_top / sc);
    let counter = words
        .iter()
        .filter(|w| {
            let t = w.text.trim();
            let b = Bx::of(w);
            w.conf >= 30.0
                && t.len() >= 3
                && t.chars().all(|c| c.is_ascii_digit())
                && b.bottom() <= rows_top + 4 * sc
                && b.h < tm.h
        })
        .max_by_key(|w| w.y)
        .map(|w| w.text.trim().to_string());

    Board {
        title,
        subtitle,
        counter,
        rows: out_rows,
    }
}

/// Lowercase, alphanumerics and single spaces: "Ninja Gaiden (NES" and
/// "Ninja Gaiden (NES)" are the same title.
pub fn normalise_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

/// Do two game names refer to the same game? Tesseract mangles a character
/// here and there, so compare on lowercase alphanumerics and accept a small
/// edit distance relative to length.
pub fn game_matches(detected: &str, configured: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let (a, b) = (norm(detected), norm(configured));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.contains(&b) || b.contains(&a) {
        return true;
    }
    // Levenshtein, capped: names of different games differ by far more.
    let (m, n) = (a.len(), b.len());
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
    prev[n] * 5 <= m.max(n)
}

/// The (game, category) a board's runs would be filed under. The configured
/// game by fuzzy match first; then a `[[games]]` alias whose `match` string
/// the normalised title contains; else the title itself (original casing,
/// punctuation dropped, single spaces) with the subtitle as the category
/// when it has three letters, "unknown" otherwise. None without a title.
pub fn canonical_key(board: &Board, cfg: &Config) -> Option<(String, String)> {
    let title = board.title.as_deref()?;
    if game_matches(title, &cfg.game.name) {
        return Some((cfg.game.name.clone(), cfg.game.category.clone()));
    }
    let norm = normalise_title(title);
    for alias in &cfg.games {
        if alias
            .r#match
            .iter()
            .map(|m| normalise_title(m))
            .any(|m| !m.is_empty() && norm.contains(&m))
        {
            return Some((
                alias.name.clone(),
                alias
                    .category
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ));
        }
    }
    let cased: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let cased = cased.split_whitespace().collect::<Vec<_>>().join(" ");
    if cased.is_empty() {
        return None;
    }
    let category = board
        .subtitle
        .as_deref()
        .filter(|s| s.chars().filter(|c| c.is_alphabetic()).count() >= 3)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| "unknown".to_string());
    Some((cased, category))
}

/// The alphabetic words of a name, normalised, when they amount to three
/// letters or more: "King Kong 2 424" is "king kong", "Act 1" is "act",
/// and "22?", "a 22?", "ex" are nothing — placeholders and fragments.
fn legible_name(name: &str) -> Option<String> {
    let n = normalise_title(name);
    let words: Vec<&str> = n
        .split(' ')
        .filter(|w| w.chars().any(char::is_alphabetic))
        .collect();
    let joined = words.join(" ");
    (joined.chars().filter(|c| c.is_alphabetic()).count() >= 3).then_some(joined)
}

/// What shadow mode records about a board: the key its runs would be filed
/// under and the rows it saw. Compared in normalised form (`normalised`),
/// so the once-a-minute reads of one and the same pane — "(NES" one
/// minute, "(NES)" the next — record one event, not hundreds.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub key: Option<(String, String)>,
    pub names: Vec<Option<String>>,
}

impl Snapshot {
    pub fn of(board: &Board, cfg: &Config) -> Self {
        Snapshot {
            key: canonical_key(board, cfg),
            names: board.rows.iter().map(|r| r.name.clone()).collect(),
        }
    }

    /// Whether the board reader is to say anything about this board:
    /// `follow_title = "log"`, and a board with a title or rows to speak of.
    pub fn wanted(board: &Board, cfg: &Config) -> bool {
        cfg.game.follow_title == FollowTitle::Log
            && (board.title.is_some() || !board.rows.is_empty())
    }

    /// The form two snapshots are compared in: the key, and the names that
    /// are legible on the board — their alphabetic words, three letters at
    /// least, sorted. Not the row count, and not what OCR made of the
    /// "???" placeholders and the highlight bar this minute: at 480p a
    /// marathon board's undrawn rows read "22?", "229", "a 22?", "ex" from
    /// one pass to the next and one of them drops out now and then, and
    /// every such variation would be an event. A game drawn onto the board
    /// is a new name, and that is a new board.
    pub fn normalised(&self) -> String {
        let key = match &self.key {
            Some((g, c)) => format!("{}|{}", normalise_title(g), normalise_title(c)),
            None => String::new(),
        };
        let mut names: Vec<String> = self
            .names
            .iter()
            .flatten()
            .filter_map(|n| legible_name(n))
            .collect();
        names.sort();
        names.dedup();
        format!("{key}#{}", names.join("/"))
    }

    /// The session event's detail: `{"key":[game,category],"rows":n,"names":[...]}`.
    pub fn json(&self) -> String {
        serde_json::json!({
            "key": self.key.as_ref().map(|(g, c)| serde_json::json!([g, c])),
            "rows": self.names.len(),
            "names": self.names,
        })
        .to_string()
    }

    /// "6 rows: Act 1, Act 2, ?, Act 4" for the log line.
    pub fn describe(&self) -> String {
        let names: Vec<&str> = self
            .names
            .iter()
            .map(|n| n.as_deref().unwrap_or("?"))
            .collect();
        format!("{} rows: {}", self.names.len(), names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GameAlias;
    use serde::Deserialize;

    fn word(x: u32, y: u32, w: u32, h: u32, text: &str) -> ocr::Word {
        ocr::Word {
            x,
            y,
            w,
            h,
            conf: 90.0,
            text: text.into(),
        }
    }

    /// A one-game pane at 2x: six rows 40 px apart under a title, the
    /// timer below. Cells are (segment, cumulative); `delta` adds a third
    /// column on the left of the rows that have one.
    fn pane(rows: &[(&str, Option<&str>, &str, &str)]) -> (Vec<ocr::Word>, Vec<ocr::Word>, R) {
        let mut words = Vec::new();
        let mut letters = vec![
            word(150, 20, 100, 24, "Ninja"),
            word(260, 20, 120, 24, "Gaiden"),
            word(390, 20, 90, 24, "(NES)"),
            word(200, 56, 60, 20, "Any%"),
        ];
        words.push(word(420, 56, 70, 20, "96326"));
        letters.push(word(420, 56, 70, 20, "96326"));
        for (i, (name, delta, seg, cum)) in rows.iter().enumerate() {
            let y = 100 + i as u32 * 40;
            letters.push(word(30, y, 40, 20, name));
            if let Some(d) = delta {
                words.push(word(180, y, 40, 20, d.trim_start_matches(['+', '-'])));
                letters.push(word(180, y, 40, 20, d));
            }
            for (x, t) in [(260, seg), (400, cum)] {
                if dash_only(t) {
                    letters.push(word(x + 60, y + 8, 14, 5, t));
                } else {
                    words.push(word(x, y, 74, 20, t));
                    letters.push(word(x, y, 74, 20, t));
                }
            }
        }
        (words, letters, (100, 200, 140, 40))
    }

    #[test]
    fn reads_a_plain_pane() {
        let (words, letters, timer) = pane(&[
            ("Act", None, "0:47.5", "0:47.5"),
            ("Act", None, "1:53.8", "2:41.4"),
            ("Act", None, "1:21.2", "4:02.7"),
        ]);
        let b = read_board(&words, &letters, 2, timer);
        assert_eq!(b.title.as_deref(), Some("Ninja Gaiden (NES)"));
        assert_eq!(b.subtitle.as_deref(), Some("Any%"));
        assert_eq!(b.counter.as_deref(), Some("96326"));
        assert_eq!(b.rows.len(), 3);
        assert_eq!(b.rows[1].name.as_deref(), Some("Act"));
        assert_eq!(b.rows[1].cells, ["1:53.8", "2:41.4"]);
        assert_eq!(b.rows[1].y, (100 + 40) / 2, "row top in 1x pixels");
        assert!(b.rows.windows(2).all(|p| p[0].y < p[1].y));
    }

    #[test]
    fn a_printed_delta_is_the_first_cell_with_its_sign() {
        let (words, letters, timer) = pane(&[
            ("Act", Some("+0.3"), "0:47.5", "0:47.5"),
            ("Act", Some("-1.2"), "1:53.8", "2:41.4"),
            ("Act", None, "1:21.2", "4:02.7"),
        ]);
        let b = read_board(&words, &letters, 2, timer);
        assert_eq!(b.rows[0].cells, ["+0.3", "0:47.5", "0:47.5"]);
        assert_eq!(b.rows[1].cells, ["-1.2", "1:53.8", "2:41.4"]);
        assert_eq!(b.rows[2].cells, ["1:21.2", "4:02.7"]);
        // The name stops at the first cell, delta included.
        assert!(b.rows.iter().all(|r| r.name.as_deref() == Some("Act")));
        // The digits pass alone (no sign read) still makes it a cell.
        let (words, _, timer) = pane(&[
            ("Act", Some("+0.3"), "0:47.5", "0:47.5"),
            ("Act", None, "1:53.8", "2:41.4"),
        ]);
        let b = read_board(&words, &[], 2, timer);
        assert_eq!(b.rows[0].cells, ["0.3", "0:47.5", "0:47.5"]);
    }

    #[test]
    fn dashes_in_a_time_column_are_placeholder_cells() {
        // A marathon board: three games done, the fourth running with "-"
        // in both columns, two more not drawn. The completed rows print a
        // bare "-" in the delta column, which is not a cell.
        let (mut words, mut letters, timer) = pane(&[
            ("Astyanax", None, "21:48", "21:48"),
            ("Kong", None, "4:24", "26:12"),
            ("SMB3", None, "1:03:20", "1:29:33"),
            ("Chip", None, "-", "-"),
            ("???", None, "-", "-"),
            ("???", None, "-", "-"),
        ]);
        for y in [100, 140, 180] {
            letters.push(word(200, y + 8, 14, 5, "-"));
        }
        words.retain(|w| w.text != "96326");
        letters.retain(|w| w.text != "96326");
        let b = read_board(&words, &letters, 2, timer);
        let names: Vec<Option<&str>> = b.rows.iter().map(|r| r.name.as_deref()).collect();
        assert_eq!(
            names,
            [
                Some("Astyanax"),
                Some("Kong"),
                Some("SMB3"),
                Some("Chip"),
                Some("???"),
                Some("???")
            ]
        );
        assert_eq!(
            b.rows[0].cells,
            ["21:48", "21:48"],
            "the delta dash is not a cell"
        );
        assert_eq!(b.rows[2].cells, ["1:03:20", "1:29:33"]);
        assert_eq!(b.rows[3].cells, ["-", "-"]);
        assert_eq!(b.rows[5].cells, ["-", "-"]);
        assert_eq!(b.counter, None);
    }

    #[test]
    fn name_only_lines_on_the_pitch_are_rows_and_others_are_not() {
        // The dashes of the undrawn rows went unread (they usually do at
        // 480p): the "???" lines still sit one pitch apart under the rows.
        let (words, mut letters, timer) = pane(&[
            ("Blaster", None, "33:33", "54:13"),
            ("DuckTales", None, "9:14", "1:03:28"),
        ]);
        letters.push(word(30, 180, 40, 20, "Arkista's"));
        letters.push(word(80, 180, 40, 20, "Ring"));
        letters.push(word(30, 220, 40, 20, "222"));
        letters.push(word(30, 260, 40, 20, "22?"));
        // Off the grid (18 px under a row's place), and reaching into the
        // columns: junk under the pane, not rows.
        letters.push(word(30, 318, 40, 20, "Cs"));
        letters.push(word(30, 340, 400, 20, "preiea"));
        let b = read_board(&words, &letters, 2, timer);
        let names: Vec<Option<&str>> = b.rows.iter().map(|r| r.name.as_deref()).collect();
        assert_eq!(
            names,
            [
                Some("Blaster"),
                Some("DuckTales"),
                Some("Arkista's Ring"),
                Some("222"),
                Some("22?")
            ]
        );
        assert!(b.rows[2].cells.is_empty());
        // With a single row there is no pitch, and no name-only rows.
        let (words, mut letters, timer) = pane(&[("Blaster", None, "33:33", "54:13")]);
        letters.push(word(30, 140, 40, 20, "222"));
        assert_eq!(read_board(&words, &letters, 2, timer).rows.len(), 1);
    }

    #[test]
    fn the_timer_leaking_above_its_crop_is_not_a_row() {
        let (mut words, letters, timer) = pane(&[
            ("Act", None, "0:47.5", "0:47.5"),
            ("Act", None, "1:53.8", "2:41.4"),
        ]);
        // The timer's digits, twice a row's height, straddling the crop top.
        words.push(word(120, 150, 240, 60, "1:35.12"));
        let b = read_board(&words, &letters, 2, timer);
        assert_eq!(b.rows.len(), 2);
    }

    #[test]
    fn an_empty_pane_reads_as_nothing() {
        let b = read_board(&[], &[], 2, (100, 200, 140, 40));
        assert_eq!(b, Board::default());
        assert_eq!(canonical_key(&b, &Config::for_test_with_min_final(1)), None);
    }

    #[test]
    fn normalise_title_drops_case_and_punctuation() {
        assert_eq!(normalise_title("Ninja Gaiden (NES)"), "ninja gaiden nes");
        assert_eq!(normalise_title("Ninja Gaiden (NES"), "ninja gaiden nes");
        assert_eq!(normalise_title("  Batman:   ROTJ "), "batman rotj");
        assert_eq!(normalise_title("Chip 'n Dale 2"), "chip n dale 2");
        assert_eq!(normalise_title("Arcathlon #6"), "arcathlon 6");
        assert_eq!(normalise_title("???"), "");
    }

    #[test]
    fn game_name_matching_tolerates_ocr_damage() {
        assert!(game_matches("Ninja Gaiden (NES)", "Ninja Gaiden (NES)"));
        assert!(game_matches("Ninja Gaiden", "Ninja Gaiden (NES)"));
        assert!(game_matches("Ninja Gaiden (NES}", "Ninja Gaiden (NES)"));
        assert!(game_matches("Ninia Gaiden (NES)", "Ninja Gaiden (NES)"));
        assert!(!game_matches("Super Mario Bros.", "Ninja Gaiden (NES)"));
        assert!(!game_matches("Ninja Gaiden II", "Castlevania"));
        assert!(!game_matches("", "Ninja Gaiden (NES)"));
    }

    fn cfg_with_alias() -> Config {
        let mut cfg = Config::for_test_with_min_final(660_000);
        cfg.game.name = "Ninja Gaiden (NES)".into();
        cfg.game.category = "Any%".into();
        cfg.games = vec![GameAlias {
            name: "Arcathlon".into(),
            category: Some("10 games".into()),
            r#match: vec!["arcath".into()],
        }];
        cfg
    }

    fn titled(title: &str, subtitle: Option<&str>) -> Board {
        Board {
            title: Some(title.into()),
            subtitle: subtitle.map(Into::into),
            ..Board::default()
        }
    }

    #[test]
    fn canonical_key_prefers_the_configured_game_then_aliases_then_the_title() {
        let cfg = cfg_with_alias();
        let ng = Some(("Ninja Gaiden (NES)".to_string(), "Any%".to_string()));
        // (a) the configured game, as tesseract spells it on a bad day.
        assert_eq!(canonical_key(&titled("Ninja Garden (NES)", None), &cfg), ng);
        assert_eq!(
            canonical_key(&titled("Ninja Gaiden (NES", Some("Any%")), &cfg),
            ng
        );
        // (b) an alias, by substring of the normalised title.
        let arca = Some(("Arcathlon".to_string(), "10 games".to_string()));
        assert_eq!(
            canonical_key(&titled("Randomized Arcathion", None), &cfg),
            arca
        );
        assert_eq!(
            canonical_key(&titled("Arcathton £4", Some("x")), &cfg),
            arca
        );
        // (c) the title itself: three misread letters are one too many for
        // the fuzzy match (the title gate treats this spelling as another
        // game too), so it files under its own name; the subtitle is the
        // category when it has three letters.
        assert_eq!(
            canonical_key(&titled("Nlnja Galden (NE5)", Some("Any%")), &cfg),
            Some(("Nlnja Galden NE5".to_string(), "Any%".to_string()))
        );
        assert_eq!(
            canonical_key(&titled("Nlnja Galden (NE5)", Some("#6")), &cfg),
            Some(("Nlnja Galden NE5".to_string(), "unknown".to_string()))
        );
        assert_eq!(
            canonical_key(&titled("Some  Other   Game", None), &cfg),
            Some(("Some Other Game".to_string(), "unknown".to_string()))
        );
        // An alias without a category files as unknown.
        let mut cfg2 = cfg.clone();
        cfg2.games[0].category = None;
        assert_eq!(
            canonical_key(&titled("Randomized Arcathlon", None), &cfg2),
            Some(("Arcathlon".to_string(), "unknown".to_string()))
        );
    }

    #[test]
    fn snapshots_compare_normalised_and_serialise_compactly() {
        let cfg = cfg_with_alias();
        let mut a = titled("Ninja Gaiden (NES", Some("Any%"));
        a.rows = vec![
            BoardRow {
                name: Some("Act 1".into()),
                cells: vec!["0:47.5".into()],
                y: 10,
            },
            BoardRow {
                name: None,
                cells: vec![],
                y: 30,
            },
        ];
        let mut b = titled("Ninja Gaiden (NES)", Some("Any%"));
        b.rows = a.rows.clone();
        b.rows[0].name = Some("ACT 1".into());
        let (sa, sb) = (Snapshot::of(&a, &cfg), Snapshot::of(&b, &cfg));
        assert_eq!(sa.normalised(), sb.normalised());
        assert_eq!(
            sa.json(),
            r#"{"key":["Ninja Gaiden (NES)","Any%"],"names":["Act 1",null],"rows":2}"#
        );
        assert_eq!(sa.describe(), "2 rows: Act 1, ?");
        // Neither the row count nor what OCR made of a placeholder row is a
        // new board: the undrawn rows of a marathon board read differently
        // every minute. A legible name is, and so is another key.
        b.rows.pop();
        assert_eq!(sa.normalised(), Snapshot::of(&b, &cfg).normalised());
        for junk in ["22?", "229", "a 22?", "ex", "as\" 222"] {
            b.rows.push(BoardRow {
                name: Some(junk.into()),
                cells: vec![],
                y: 50,
            });
            assert_eq!(
                sa.normalised(),
                Snapshot::of(&b, &cfg).normalised(),
                "{junk:?}"
            );
        }
        b.rows[0].name = Some("Act 1 424".into());
        assert_eq!(sa.normalised(), Snapshot::of(&b, &cfg).normalised());
        b.rows[0].name = Some("Batman: ROTJ".into());
        assert_ne!(sa.normalised(), Snapshot::of(&b, &cfg).normalised());
        let c = titled("Randomized Arcathlon", None);
        assert_ne!(sa.normalised(), Snapshot::of(&c, &cfg).normalised());
        assert_eq!(legible_name("King Kong 2 424"), Some("king kong".into()));
        assert_eq!(
            legible_name("SMB3 (Warpless)"),
            Some("smb3 warpless".into())
        );
        assert_eq!(legible_name("???"), None);
        assert_eq!(legible_name("eo 22?"), None);
        assert_eq!(
            Snapshot::of(&Board::default(), &cfg).json(),
            r#"{"key":null,"names":[],"rows":0}"#
        );
        // Only spoken of in shadow mode, and only when there is something to say.
        assert!(!Snapshot::wanted(&a, &cfg));
        let mut cfg = cfg;
        cfg.game.follow_title = FollowTitle::Log;
        assert!(Snapshot::wanted(&a, &cfg));
        assert!(!Snapshot::wanted(&Board::default(), &cfg));
    }

    // ---- real panes: tests/fixtures/board/*.json (see its README.md)

    #[derive(Deserialize)]
    struct Fixture {
        name: String,
        scale: u32,
        timer: R,
        words: Vec<FWord>,
        letters: Vec<FWord>,
        expected: Expected,
    }

    #[derive(Deserialize)]
    struct FWord {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        conf: f32,
        text: String,
    }

    #[derive(Deserialize)]
    struct Expected {
        title: Option<String>,
        subtitle: Option<String>,
        counter: Option<String>,
        rows: Vec<ERow>,
    }

    #[derive(Deserialize)]
    struct ERow {
        name: Option<String>,
        cells: Vec<Option<String>>,
    }

    fn load(name: &str) -> Fixture {
        let path = format!(
            "{}/tests/fixtures/board/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn to_words(v: &[FWord]) -> Vec<ocr::Word> {
        v.iter()
            .map(|w| ocr::Word {
                x: w.x,
                y: w.y,
                w: w.w,
                h: w.h,
                conf: w.conf,
                text: w.text.clone(),
            })
            .collect()
    }

    /// How close the read title has to come to the truth.
    #[derive(Clone, Copy)]
    enum Title {
        /// No title on the pane, none read.
        Absent,
        /// The fuzzy match the title gate uses (a letter or two off).
        Fuzzy,
        /// Every word read is a word of the truth or the start of one: the
        /// image lost part of the title (a word under the confidence
        /// threshold, a line cut by the crop), what was read is right.
        Words,
    }

    struct Tolerance {
        title: Title,
        /// Least share of the named rows whose name must match.
        names: f64,
        /// Least share of the expected time cells that must be read exactly.
        cells: f64,
    }

    fn alnum(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }

    fn name_matches(read: &str, expected: &str) -> bool {
        if normalise_title(read) == normalise_title(expected) {
            return true;
        }
        if alnum(expected).is_empty() {
            // LiveSplit's "???" for a game not drawn yet: tesseract reads
            // the question marks as 2s (and once a 7).
            return !read.is_empty() && read.chars().all(|c| matches!(c, '?' | '2' | '7'));
        }
        game_matches(read, expected)
    }

    fn title_ok(read: Option<&str>, expected: Option<&str>, how: Title) -> bool {
        match (how, read, expected) {
            (Title::Absent, None, None) => true,
            (Title::Absent, ..) => false,
            (_, None, _) | (_, _, None) => false,
            (Title::Fuzzy, Some(r), Some(e)) => game_matches(r, e),
            (Title::Words, Some(r), Some(e)) => {
                let e = normalise_title(e);
                let ew: Vec<&str> = e.split(' ').collect();
                let r = normalise_title(r);
                let rw: Vec<&str> = r.split(' ').filter(|w| !w.is_empty()).collect();
                !rw.is_empty() && rw.iter().all(|w| ew.iter().any(|x| x.starts_with(w)))
            }
        }
    }

    fn check(name: &str, tol: Tolerance) {
        let f = load(name);
        assert_eq!(f.name, name);
        let b = read_board(&to_words(&f.words), &to_words(&f.letters), f.scale, f.timer);
        let e = &f.expected;
        eprintln!(
            "{name}: title {:?} [{:?}] counter {:?}",
            b.title, b.subtitle, b.counter
        );
        for r in &b.rows {
            eprintln!(
                "  y={:>4} {:<22} {:?}",
                r.y,
                r.name.as_deref().unwrap_or("?"),
                r.cells
            );
        }
        assert!(
            title_ok(b.title.as_deref(), e.title.as_deref(), tol.title),
            "{name}: title {:?}, expected {:?}",
            b.title,
            e.title
        );
        if let (Some(s), Some(es)) = (&b.subtitle, &e.subtitle) {
            assert_eq!(normalise_title(s), normalise_title(es), "{name}: subtitle");
        }
        assert_eq!(b.counter, e.counter, "{name}: counter");
        assert_eq!(
            b.rows.len(),
            e.rows.len(),
            "{name}: {} rows read, {} on the pane",
            b.rows.len(),
            e.rows.len()
        );
        let (mut named, mut name_hits) = (0usize, 0usize);
        let (mut times, mut time_hits) = (0usize, 0usize);
        let mut misses: Vec<String> = Vec::new();
        for (i, (got, want)) in b.rows.iter().zip(&e.rows).enumerate() {
            if let Some(w) = &want.name {
                named += 1;
                match &got.name {
                    Some(g) if name_matches(g, w) => name_hits += 1,
                    other => misses.push(format!("row {i} name {other:?} != {w:?}")),
                }
            }
            let mut from = 0;
            for cell in want.cells.iter().flatten().filter(|c| time_shaped(c)) {
                times += 1;
                match got.cells[from..].iter().position(|c| c == cell) {
                    Some(p) => {
                        time_hits += 1;
                        from += p + 1;
                    }
                    None => misses.push(format!("row {i} cell {cell:?} not in {:?}", got.cells)),
                }
            }
        }
        for m in &misses {
            eprintln!("  miss: {m}");
        }
        let ratio = |hits: usize, total: usize| {
            if total == 0 {
                1.0
            } else {
                hits as f64 / total as f64
            }
        };
        assert!(
            ratio(name_hits, named) >= tol.names,
            "{name}: {name_hits}/{named} names matched"
        );
        assert!(
            ratio(time_hits, times) >= tol.cells,
            "{name}: {time_hits}/{times} time cells read exactly"
        );
    }

    #[test]
    fn fixture_arcathlon_final() {
        // Clean 1080p board, ten games: everything read exactly. The
        // title's second word came back at confidence 29.0, under the 30
        // the title lines take, so only "Randomized" is read.
        check(
            "arcathlon-final",
            Tolerance {
                title: Title::Words,
                names: 1.0,
                cells: 1.0,
            },
        );
    }

    #[test]
    fn fixture_jul14_gameplay() {
        // Clean pane, timer not started: every row, name and time. "Gaiden"
        // was read as "Cake" at confidence 17, below the title threshold.
        check(
            "jul14-gameplay",
            Tolerance {
                title: Title::Words,
                names: 1.0,
                cells: 1.0,
            },
        );
    }

    #[test]
    fn fixture_jul14_opening() {
        // The opening scene's oversized, right-clipped pane: no title in
        // the crop, the cumulative column cut to "0:4" (null in the truth,
        // not a time), six segment times readable.
        check(
            "jul14-opening",
            Tolerance {
                title: Title::Absent,
                names: 1.0,
                cells: 1.0,
            },
        );
    }

    #[test]
    fn fixture_ng_default() {
        // Mid-run, Act 5 highlighted: its label went unread (5/6 names), and
        // the highlighted cumulative cell merged with the highlight bar
        // (11/12 times). The crop's top edge cuts the title to "Nin; Gaid".
        check(
            "ng-default",
            Tolerance {
                title: Title::Words,
                names: 0.8,
                cells: 0.8,
            },
        );
    }

    #[test]
    fn fixture_ng_theme() {
        // The NES-styled scene at 480p: the highlight bar swallows two
        // labels and the pane border reads as junk glyphs (4/6 names), a
        // colon lost in "241.9" and the highlighted segment time unread
        // (10/12 times).
        check(
            "ng-theme",
            Tolerance {
                title: Title::Fuzzy,
                names: 0.5,
                cells: 0.8,
            },
        );
    }

    #[test]
    fn fixture_arcathlon_numbered() {
        // "Arcathlon #6" over background art: every name; one cumulative
        // cell merged with the art into a box twice the height (19/20).
        check(
            "arcathlon-numbered",
            Tolerance {
                title: Title::Fuzzy,
                names: 1.0,
                cells: 0.9,
            },
        );
    }

    #[test]
    fn fixture_arcathlon_early() {
        // Three games done, one running, six "???" to come: the digits pass
        // merged each row's times into one word, so the letters pass reads
        // them; the running and undrawn rows have no time at all and are
        // found by the pitch alone. The dashes were not read at 480p.
        check(
            "arcathlon-early",
            Tolerance {
                title: Title::Fuzzy,
                names: 1.0,
                cells: 1.0,
            },
        );
    }
}
