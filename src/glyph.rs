//! A purpose-built reader for the LiveSplit timer's digits.
//!
//! Tesseract is a general OCR engine pointed at a very narrow problem: one
//! font, a fixed grammar (`M:SS.hh`), digits that never touch. Every timer
//! misread the tracker has had to defend against — a 1 read as a 7, a phantom
//! trailing digit, a dropped leading one, the hundredths' point lost, a
//! threshold that suits one theme and not another — is tesseract guessing
//! where a fixed font leaves nothing to guess. This reader instead:
//!
//! 1. finds the digit band (the rows holding the most ink, as `ink_extent`
//!    does) and cuts it into glyphs at the gaps between ink columns;
//! 2. resamples each glyph onto a small fixed grid, as grayscale — no hard
//!    threshold, so it is indifferent to how bright a theme draws its digits;
//! 3. matches it by normalised correlation against templates harvested from
//!    this streamer's own footage (frames whose tesseract reading the tracker
//!    later confirmed), and declines when the best match is poor or two
//!    classes are close — an unreadable frame stays unreadable, never wrong;
//! 4. assembles the characters left to right and checks them against the
//!    timer grammar.
//!
//! Microseconds per frame, deterministic, and testable against saved crops.

use anyhow::{bail, Context, Result};
use image::GrayImage;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::warn;

/// Glyph feature grid.
pub const GW: usize = 12;
pub const GH: usize = 20;
const N: usize = GW * GH;

/// Characters the reader knows.
pub const CLASSES: &[char] = &['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', '.'];

/// One glyph cut from the crop, in crop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub class: char,
    /// Zero-mean, unit-norm grayscale on the GW x GH grid.
    pub v: Vec<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateSet {
    /// Where these came from (layout names), for the record.
    #[serde(default)]
    pub sources: Vec<String>,
    /// The ink level the training crops were segmented at (0: unknown).
    #[serde(default)]
    pub level: u8,
    pub templates: Vec<Template>,
}

impl TemplateSet {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading glyph templates {}", path.display()))?;
        let set: TemplateSet = serde_json::from_str(&text).context("parsing glyph templates")?;
        if set.templates.is_empty() {
            bail!("{} holds no templates", path.display());
        }
        for (i, t) in set.templates.iter().enumerate() {
            if t.v.len() != N {
                bail!(
                    "{}: template {i} has {} values, the {GW}x{GH} grid needs {N}",
                    path.display(),
                    t.v.len()
                );
            }
            if !CLASSES.contains(&t.class) {
                bail!(
                    "{}: template {i} is for {:?}, which the reader does not know",
                    path.display(),
                    t.class
                );
            }
        }
        Ok(set)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Four decimals of a unit-norm vector are far below the noise in
        // any frame, and a third the file size of full-precision floats.
        let mut rounded = self.clone();
        for t in &mut rounded.templates {
            for x in &mut t.v {
                *x = (*x * 10_000.0).round() / 10_000.0;
            }
        }
        std::fs::write(path, serde_json::to_string(&rounded)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn count(&self, class: char) -> usize {
        self.templates.iter().filter(|t| t.class == class).count()
    }
}

/// Column-wise ink in the crop: how many pixels of each column are brighter
/// than `level` (the timer is light on dark; the caller passes the bright
/// channel).
fn column_ink(img: &GrayImage, level: u8, rows: std::ops::Range<u32>) -> Vec<u32> {
    let (w, _) = img.dimensions();
    let mut cols = vec![0u32; w as usize];
    for y in rows {
        for x in 0..w {
            if img.get_pixel(x, y).0[0] > level {
                cols[x as usize] += 1;
            }
        }
    }
    cols
}

/// The columns that hold the pane, [start, end): a pane border's ink touches
/// both the top and the bottom edge of the crop, which no digit ever does
/// (digits sit in a band with margins). A border in the left quarter is a
/// left edge and the pane starts after it; the first border past that is the
/// right edge, and whatever lies beyond it (the game, often bright) is not
/// ours.
fn pane_span(img: &GrayImage, level: u8) -> (usize, usize) {
    let (w, h) = img.dimensions();
    let edge_to_edge = |x: u32| {
        let inked = |y: u32| img.get_pixel(x, y).0[0] > level;
        (0..3.min(h)).any(inked) && (h.saturating_sub(3)..h).any(inked)
    };
    let mut start = 0usize;
    let mut end = w as usize;
    for x in 0..w {
        if edge_to_edge(x) {
            if (x as usize) < w as usize / 4 {
                start = x as usize + 1;
            } else {
                end = x as usize;
                break;
            }
        }
    }
    // The border's anti-aliased inner edge — a few columns, partially inked,
    // fading across them — falls short of the crop's top and bottom rows, so
    // the test above does not catch it, and it is contiguous with whatever
    // glyph ends the timer: merged into that glyph's box it turns a small
    // "0" into a "9". The timer keeps at least four columns from the border,
    // so a fixed margin inside each border clears the sliver without
    // touching a digit.
    if end < w as usize {
        end = end.saturating_sub(4).max(start);
    }
    if start > 0 {
        start = (start + 4).min(end);
    }
    (start, end)
}

/// The digit band: the run of rows holding the most ink, bridging gaps of up
/// to `bridge` rows (digits are vertically solid; the small hundredths sit
/// within the big digits' rows). Thin bands (separator lines) are ignored.
fn digit_band(img: &GrayImage, level: u8) -> Option<(u32, u32)> {
    let (w, h) = img.dimensions();
    let cols = column_ink(img, level, 0..h);
    let (start, end) = pane_span(img, level);
    let is_digit_col = |x: usize| x >= start && x < end && cols[x] >= 2;
    let mut rows = vec![0u32; h as usize];
    for y in 0..h {
        for x in 0..w {
            if is_digit_col(x as usize) && img.get_pixel(x, y).0[0] > level {
                rows[y as usize] += 1;
            }
        }
    }
    let min = 2u32;
    let bridge = 2usize;
    let mut best: Option<(u32, u32, u64)> = None;
    let mut y = 0usize;
    while y < h as usize {
        if rows[y] < min {
            y += 1;
            continue;
        }
        let top = y;
        let (mut ink, mut gap, mut bottom) = (0u64, 0usize, y);
        while y < h as usize && gap <= bridge {
            if rows[y] >= min {
                ink += rows[y] as u64;
                bottom = y;
                gap = 0;
            } else {
                gap += 1;
            }
            y += 1;
        }
        if bottom - top + 1 > 3 && best.is_none_or(|b| ink > b.2) {
            best = Some((top as u32, bottom as u32, ink));
        }
    }
    best.map(|(t, b, _)| (t, b))
}

/// Cut the digit band into glyph boxes at the gaps between ink columns.
pub fn segment(img: &GrayImage, level: u8) -> Vec<GlyphBox> {
    let Some((top, bottom)) = digit_band(img, level) else {
        return Vec::new();
    };
    let cols = column_ink(img, level, top..bottom + 1);
    let band_h = bottom - top + 1;
    let boxes = raw_boxes(img, level, (top, bottom), &cols);
    // Two digits that touch (anti-aliasing across a tight kerning — the
    // small hundredths pair does this often) make one box about twice as
    // wide as a digit of that height. A digit is roughly two thirds as wide
    // as it is tall; a box much wider than that is cut into as many pieces
    // as fit, at the thinnest column near each expected boundary. (The
    // reader itself cuts by trying the templates on every candidate cut —
    // see `GlyphReader::refine` — this geometric cut serves training and
    // diagnostics, where there are no templates yet.)
    let mut out = Vec::with_capacity(boxes.len() + 2);
    for b in boxes {
        let expected = (b.h * 2 / 3).max(4);
        // Only digit-height boxes: the point and the colon are wider than
        // two thirds of their own height by nature.
        let digit_height = b.h * 5 >= band_h * 2;
        if digit_height && b.w > expected * 3 / 2 && b.w >= 8 {
            let n = ((b.w + expected / 2) / expected).clamp(2, 4) as usize;
            let mut x0 = b.x as usize;
            for k in 1..=n {
                let x1 = if k == n {
                    (b.x + b.w) as usize
                } else {
                    let target = b.x as usize + b.w as usize * k / n;
                    let lo = target.saturating_sub(expected as usize / 3).max(x0 + 2);
                    let hi = (target + expected as usize / 3).min((b.x + b.w) as usize - 2);
                    if lo < hi {
                        (lo..hi).min_by_key(|&c| cols[c]).unwrap_or(target)
                    } else {
                        target
                    }
                };
                if x1 > x0 {
                    out.push(GlyphBox {
                        x: x0 as u32,
                        y: b.y,
                        w: (x1 - x0) as u32,
                        h: b.h,
                    });
                }
                x0 = x1;
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// One box per run of inked columns inside the pane, each tightened to its
/// own rows; touching glyphs come out as one box. `cols` is the column ink
/// over the digit band `(top, bottom)`, both inclusive.
fn raw_boxes(img: &GrayImage, level: u8, band: (u32, u32), cols: &[u32]) -> Vec<GlyphBox> {
    let (start, end) = pane_span(img, level);
    let mut boxes = Vec::new();
    let mut x = start;
    while x < end {
        if cols[x] == 0 {
            x += 1;
            continue;
        }
        let x0 = x;
        // A glyph ends at a fully empty column; one empty column inside a
        // glyph would be unusual for this font, so no bridging.
        while x < end && cols[x] > 0 {
            x += 1;
        }
        // A sliver a few pixels wide is the anti-aliased inner edge of the
        // border or a speck, never a glyph: the narrowest real glyph, the
        // point, is ~10px wide at canvas scale.
        if x - x0 > 4 {
            if let Some(b) = tight_box(img, level, x0, x, band, cols) {
                boxes.push(b);
            }
        }
    }
    boxes
}

/// The box of the ink in columns `x0..x1` of the band: trimmed to the inked
/// columns and rows, or None when there is no ink there.
fn tight_box(
    img: &GrayImage,
    level: u8,
    mut x0: usize,
    mut x1: usize,
    band: (u32, u32),
    cols: &[u32],
) -> Option<GlyphBox> {
    let (top, bottom) = band;
    while x0 < x1 && cols[x0] == 0 {
        x0 += 1;
    }
    while x1 > x0 && cols[x1 - 1] == 0 {
        x1 -= 1;
    }
    if x0 >= x1 {
        return None;
    }
    let (mut gy0, mut gy1) = (bottom, top);
    for yy in top..=bottom {
        for xx in x0..x1 {
            if img.get_pixel(xx as u32, yy).0[0] > level {
                gy0 = gy0.min(yy);
                gy1 = gy1.max(yy);
                break;
            }
        }
    }
    (gy1 >= gy0).then_some(GlyphBox {
        x: x0 as u32,
        y: gy0,
        w: (x1 - x0) as u32,
        h: gy1 - gy0 + 1,
    })
}

/// The vertical extent shared by all glyphs of a crop: the digit band, as
/// the union of the glyph boxes.
pub fn band_of(boxes: &[GlyphBox]) -> Option<(u32, u32)> {
    let top = boxes.iter().map(|b| b.y).min()?;
    let bottom = boxes.iter().map(|b| b.y + b.h).max()?;
    Some((top, bottom))
}

/// The frame a glyph is classified in: the full band height, and a width of
/// at least 60% of it, centred on the glyph. Classifying the tight box alone
/// would leave a "1" (a solid bar) and a "." with no internal structure at
/// all, and would throw away where in the band a glyph sits — which is what
/// tells a colon from a point and a small hundredths digit from a big one.
pub fn frame_for(b: GlyphBox, band: (u32, u32), img_w: u32) -> GlyphBox {
    let (top, bottom) = band;
    let band_h = bottom.saturating_sub(top).max(1);
    // Big digits fill the band; the small hundredths digits are about half
    // its height and sit on the baseline; the point is smaller still. Each
    // gets a frame of its own height (at least half the band, so a point is
    // "a blob at the bottom" rather than a featureless block), bottom-aligned
    // with the glyph, so a small digit is classified at full resolution and
    // still looks nothing like a big one.
    let fh = b.h.max(band_h / 2).max(1);
    // Digits get a frame at least 60% as wide as tall (so a "1" has a dark
    // field around its bar); a point or colon, which are far shorter than
    // the band, keep a tight frame — a wider one takes in the edges of the
    // digits either side, which vary with what those digits are.
    let fw = if b.h * 2 < band_h {
        b.w + 2
    } else {
        b.w.max(fh * 3 / 5).max(1)
    };
    let cx = b.x as i64 + b.w as i64 / 2;
    let x = (cx - fw as i64 / 2).clamp(0, (img_w as i64 - fw as i64).max(0)) as u32;
    let y = (b.y as i64 + b.h as i64 - fh as i64).max(top as i64) as u32;
    GlyphBox {
        x,
        y,
        w: fw.min(img_w),
        h: fh,
    }
}

/// Resample a glyph onto the GW x GH grid (area averaging), then zero-mean
/// and unit-norm so correlation is indifferent to brightness and contrast.
pub fn features(img: &GrayImage, b: GlyphBox) -> Vec<f32> {
    let mut v = vec![0f32; N];
    for gy in 0..GH {
        let y0 = b.y as f32 + b.h as f32 * gy as f32 / GH as f32;
        let y1 = b.y as f32 + b.h as f32 * (gy + 1) as f32 / GH as f32;
        for gx in 0..GW {
            let x0 = b.x as f32 + b.w as f32 * gx as f32 / GW as f32;
            let x1 = b.x as f32 + b.w as f32 * (gx + 1) as f32 / GW as f32;
            // Area-weighted mean over the source pixels this cell covers.
            let (mut sum, mut wsum) = (0f32, 0f32);
            let mut py = y0.floor() as u32;
            while (py as f32) < y1 {
                let cy = (y1.min(py as f32 + 1.0) - y0.max(py as f32)).max(0.0);
                let mut px = x0.floor() as u32;
                while (px as f32) < x1 {
                    let cx = (x1.min(px as f32 + 1.0) - x0.max(px as f32)).max(0.0);
                    let wgt = cx * cy;
                    if wgt > 0.0 {
                        let p = img
                            .get_pixel(px.min(img.width() - 1), py.min(img.height() - 1))
                            .0[0] as f32;
                        sum += p * wgt;
                        wsum += wgt;
                    }
                    px += 1;
                }
                py += 1;
            }
            v[gy * GW + gx] = if wsum > 0.0 { sum / wsum } else { 0.0 };
        }
    }
    normalise(&mut v);
    v
}

fn normalise(v: &mut [f32]) {
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    for x in v.iter_mut() {
        *x -= mean;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    // A frame with no contrast to speak of — a solid blob, a patch of
    // background — has no shape to match: scaling its rounding crumbs up
    // to unit norm would make a random vector that correlates with
    // anything by chance. Leave it all zeros, which correlates with
    // nothing. Real glyphs, ink on dark, spread over 50+ levels RMS.
    if norm > 4.0 * (v.len() as f32).sqrt() {
        for x in v.iter_mut() {
            *x /= norm;
        }
    } else {
        v.fill(0.0);
    }
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Best class for a feature vector: (class, score, margin over the runner-up
/// class).
pub fn classify(set: &TemplateSet, v: &[f32]) -> Option<(char, f32, f32)> {
    let mut best: Vec<(char, f32)> = Vec::new();
    for c in CLASSES {
        let s = set
            .templates
            .iter()
            .filter(|t| t.class == *c)
            .map(|t| correlation(&t.v, v))
            .fold(f32::MIN, f32::max);
        if s > f32::MIN {
            best.push((*c, s));
        }
    }
    best.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (c, s) = *best.first()?;
    let runner = best.get(1).map(|b| b.1).unwrap_or(f32::MIN);
    Some((c, s, s - runner))
}

/// The reading of one crop.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub text: String,
    pub boxes: Vec<GlyphBox>,
    /// Lowest per-glyph score.
    pub confidence: f32,
    /// Lowest per-glyph margin over the runner-up class.
    pub margin: f32,
}

pub struct GlyphReader {
    set: TemplateSet,
    /// Ink level used for segmentation only (the classifier sees grayscale).
    level: u8,
    /// Reject a glyph whose best score is below this...
    min_score: f32,
    /// ...or whose margin over the runner-up class is below this.
    min_margin: f32,
}

impl GlyphReader {
    pub fn new(set: TemplateSet, level: u8) -> Self {
        Self {
            set,
            level,
            min_score: 0.55,
            min_margin: 0.08,
        }
    }

    pub fn load(path: &Path, level: u8) -> Result<Self> {
        let set = TemplateSet::load(path)?;
        if set.level != 0 && set.level != level {
            warn!(
                "glyph templates {} were trained at threshold {}, the timer reads at {level}: glyphs may segment differently",
                path.display(),
                set.level
            );
        }
        Ok(Self::new(set, level))
    }

    /// Read the timer crop, or None when any glyph is uncertain or the
    /// characters do not form a time.
    pub fn read(&self, img: &GrayImage) -> Option<Reading> {
        self.read_diag(img).ok()
    }

    /// `read`, with the reason for a decline.
    pub fn read_diag(&self, img: &GrayImage) -> std::result::Result<Reading, Decline> {
        let (glyphs, band) = self.glyphs(img)?;
        let band_h = band.1 - band.0;
        let mut text = String::with_capacity(glyphs.len());
        let mut boxes = Vec::with_capacity(glyphs.len());
        let mut confidence = f32::MAX;
        let mut margin = f32::MAX;
        for (b, scored) in glyphs {
            let (c, s, m) = scored.ok_or(Decline::Segmentation(0))?;
            if s < self.min_score {
                return Err(Decline::Score(c, s));
            }
            if m < self.min_margin {
                return Err(Decline::Margin(c, m));
            }
            // The minus of the pre-start countdown ("-5.00") is a blob like
            // the point, but sits mid-band where a point sits on the
            // baseline; the templates hold no minus, so it comes out as a
            // leading point. Tesseract runs with a digits-only whitelist and
            // never reports the sign, and the tracker knows the countdown
            // by its value, so drop it the same way.
            if c == '.' && text.is_empty() && (b.y + b.h / 2) < band.0 + band_h * 7 / 10 {
                continue;
            }
            confidence = confidence.min(s);
            margin = margin.min(m);
            text.push(c);
            boxes.push(b);
        }
        if !time_shaped(&text) {
            return Err(Decline::Grammar(text));
        }
        Ok(Reading {
            text,
            boxes,
            confidence,
            margin,
        })
    }

    /// The crop's glyphs as the reader cuts and scores them, left to right,
    /// with the digit band `(top, bottom)` they sit in.
    pub fn glyphs(
        &self,
        img: &GrayImage,
    ) -> std::result::Result<(Vec<(GlyphBox, Option<Scored>)>, (u32, u32)), Decline> {
        // Nothing this small holds a timer; the geometry below assumes a
        // few rows of margin and a glyph's width of columns.
        if img.width() < 8 || img.height() < 8 {
            return Err(Decline::Segmentation(0));
        }
        let (top, bottom) = digit_band(img, self.level).ok_or(Decline::Segmentation(0))?;
        let cols = column_ink(img, self.level, top..bottom + 1);
        let mut raw = raw_boxes(img, self.level, (top, bottom), &cols);
        let pane_end = pane_span(img, self.level).1 as u32;
        let band = band_of(&raw).ok_or(Decline::Segmentation(0))?;
        let band_h = band.1 - band.0;
        // What is left of the border's anti-aliased inner edge after
        // `pane_span` (its width varies with where the crop falls on the
        // pane) is a narrow column run against the pane's end — no glyph
        // ends the timer but a digit, and none is that narrow.
        if let Some(last) = raw.last() {
            if last.x + last.w >= pane_end && last.w * 5 < band_h {
                raw.pop();
            }
        }
        if raw.is_empty() || raw.len() > 10 {
            return Err(Decline::Segmentation(raw.len()));
        }
        let mut glyphs = Vec::with_capacity(raw.len() + 2);
        let n = raw.len();
        for (i, b) in raw.into_iter().enumerate() {
            let trailing = i + 1 == n && b.x + b.w >= pane_end;
            self.refine(img, &cols, band, b, 2, trailing, &mut glyphs);
        }
        if glyphs.len() > 10 {
            return Err(Decline::Segmentation(glyphs.len()));
        }
        Ok((glyphs, band))
    }

    fn score_box(&self, img: &GrayImage, band: (u32, u32), b: GlyphBox) -> Option<Scored> {
        // Specks (a few pixels) are noise, not glyphs.
        if b.w * b.h < 6 {
            return None;
        }
        classify(&self.set, &features(img, frame_for(b, band, img.width())))
    }

    /// Classify a raw box, cutting it where two glyphs that touch meet.
    ///
    /// Touching glyphs make one box: the small hundredths pair across their
    /// tight kerning, and the "4", whose crossbar overhangs the point after
    /// it. The junction is not reliably the thinnest column, so the
    /// templates decide: every cut that leaves a plausible width either
    /// side and goes through no thick stroke is tried, the right-hand piece
    /// cut again in turn, and the cut whose worst piece scores best wins —
    /// if it beats the box read whole. A cut through a digit leaves a
    /// fragment that matches nothing well, so the whole box wins there.
    ///
    /// The `trailing` box runs against the pane's end, where the border's
    /// anti-aliased inner edge can be stuck to it: a cut there may also
    /// simply discard the right-hand piece, if the digit left of it reads
    /// clearly better than the box did whole.
    fn refine(
        &self,
        img: &GrayImage,
        cols: &[u32],
        band: (u32, u32),
        b: GlyphBox,
        depth: u32,
        trailing: bool,
        out: &mut Vec<(GlyphBox, Option<Scored>)>,
    ) {
        let whole = self.score_box(img, band, b);
        let band_h = band.1 - band.0;
        let expected = (b.h * 2 / 3).max(4);
        // Only digit-height boxes are ever two glyphs: the point and the
        // colon are wider than two thirds of their height by nature.
        let digit_height = b.h * 5 >= band_h * 2;
        // The narrowest glyph, the point, is about a sixth of the band
        // tall and as wide as that.
        let min_piece = (band_h as usize / 6).max(4);
        let (x0, x1) = (b.x as usize, (b.x + b.w) as usize);
        let rows = (band.0, band.1.saturating_sub(1));
        let mut best: Option<(f32, Vec<(GlyphBox, Option<Scored>)>)> = None;
        if depth > 0 && digit_height && b.w > expected * 5 / 4 && x1 >= x0 + 2 * min_piece {
            // Every other column of a pair of digits; coarser on a box far
            // wider than any pair (a crop the threshold turned into one
            // blob), which bounds the search at about a thousand scores.
            let step = (b.w as usize / 32).max(2);
            for c in (x0 + min_piece..=x1 - min_piece).step_by(step) {
                if cols[c] * 3 > band_h {
                    continue;
                }
                let Some(l) = tight_box(img, self.level, x0, c, rows, cols) else {
                    continue;
                };
                let Some(r) = tight_box(img, self.level, c, x1, rows, cols) else {
                    continue;
                };
                let Some(ls) = self.score_box(img, band, l) else {
                    continue;
                };
                let mut pieces = vec![(l, Some(ls))];
                self.refine(img, cols, band, r, depth - 1, trailing, &mut pieces);
                let worst = pieces
                    .iter()
                    .map(|p| p.1.map_or(f32::MIN, |s| s.1))
                    .fold(f32::MAX, f32::min);
                if best.as_ref().is_none_or(|(s, _)| worst > *s) {
                    best = Some((worst, pieces));
                }
            }
        }
        if trailing && digit_height && x1 > x0 + min_piece {
            // The stuck edge is at most a fifth of the band wide. Trimming a
            // column or two off a clean digit changes its score by very
            // little, so demand a clear gain before believing there was an
            // edge to trim.
            let widest = (band_h as usize / 5).max(2);
            let floor = whole.map_or(0.0, |w| w.1 + 0.1);
            for c in (x1.saturating_sub(widest).max(x0 + min_piece)..x1).rev() {
                let Some(l) = tight_box(img, self.level, x0, c, rows, cols) else {
                    continue;
                };
                let Some(ls) = self.score_box(img, band, l) else {
                    continue;
                };
                if ls.1 > floor && best.as_ref().is_none_or(|(s, _)| ls.1 > *s) {
                    best = Some((ls.1, vec![(l, Some(ls))]));
                }
            }
        }
        match best {
            Some((s, pieces)) if whole.is_none_or(|w| s > w.1) => out.extend(pieces),
            _ => out.push((b, whole)),
        }
    }
}

/// A glyph's classification: class, score, margin over the runner-up.
type Scored = (char, f32, f32);

/// Why a crop was not read.
#[derive(Debug, Clone, PartialEq)]
pub enum Decline {
    /// No glyphs, too many, or a speck among them (count).
    Segmentation(usize),
    /// A glyph's best match scored too low (its class, the score).
    Score(char, f32),
    /// A glyph's best and second-best classes were too close.
    Margin(char, f32),
    /// The characters do not form a time.
    Grammar(String),
}

/// `M:SS.hh`, `MM:SS.hh`, `S.hh`, `SS.hh`, `H:MM:SS.hh`.
fn time_shaped(t: &str) -> bool {
    let Some((main, frac)) = t.split_once('.') else {
        // The display always has its hundredths; a reading without them
        // has lost glyphs, and tesseract may do better with the frame.
        return false;
    };
    // LiveSplit shows exactly two fraction digits; one is a dropped glyph,
    // three a spurious one — both are declines, not readings.
    if frac.len() != 2 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let parts: Vec<&str> = main.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    if parts
        .iter()
        .any(|p| p.is_empty() || p.len() > 2 || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    // Every part after the first is exactly two digits.
    parts[1..].iter().all(|p| p.len() == 2)
}

/// The text LiveSplit displays for a time, in the timer's own format —
/// what a frame's confirmed reading labels its glyphs with.
pub fn display_text(ms: i64) -> String {
    let hundredths = (ms % 1000) / 10;
    let s = ms / 1000;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{hundredths:02}")
    } else if m > 0 {
        format!("{m}:{sec:02}.{hundredths:02}")
    } else {
        format!("{sec}.{hundredths:02}")
    }
}

/// Harvest templates from a corpus: `crops` are (image, label) pairs where
/// the label is the text on screen. A crop contributes only when it segments
/// into exactly as many glyphs as the label has characters; per class, keep
/// up to `per_class` templates chosen to be mutually diverse (farthest-point
/// selection), so a few odd renderings are represented alongside the norm.
pub fn train(
    crops: &[(GrayImage, String)],
    level: u8,
    per_class: usize,
) -> (TemplateSet, TrainStats) {
    let mut pool: Vec<(char, Vec<f32>)> = Vec::new();
    let mut stats = TrainStats::default();
    for (img, label) in crops {
        let boxes = segment(img, level);
        let chars: Vec<char> = label.chars().collect();
        if boxes.len() != chars.len() {
            stats.skipped_segmentation += 1;
            if stats.mismatches.len() < 10 {
                stats.mismatches.push((
                    label.clone(),
                    boxes.iter().map(|b| (b.x, b.y, b.w, b.h)).collect(),
                ));
            }
            continue;
        }
        if chars.iter().any(|c| !CLASSES.contains(c)) {
            stats.skipped_label += 1;
            continue;
        }
        let Some(band) = band_of(&boxes) else {
            continue;
        };
        for (b, c) in boxes.iter().zip(chars) {
            if b.w * b.h < 6 {
                continue;
            }
            pool.push((c, features(img, frame_for(*b, band, img.width()))));
        }
        stats.used += 1;
    }
    let mut set = TemplateSet {
        level,
        ..Default::default()
    };
    for &c in CLASSES {
        let cands: Vec<&Vec<f32>> = pool
            .iter()
            .filter(|(k, _)| *k == c)
            .map(|(_, v)| v)
            .collect();
        if cands.is_empty() {
            continue;
        }
        // Labels are only as good as the readings behind them, so a class's
        // examples include a few that are really something else. Rank every
        // example by how typical it is of the class (correlation with the
        // class centroid), drop the least typical third, and store the
        // centroid itself plus examples spread across the typical ones —
        // both size variants of a digit, both themes — chosen by
        // farthest-point selection among what remains.
        let mut centroid = vec![0f32; N];
        for v in &cands {
            for (cc, x) in centroid.iter_mut().zip(v.iter()) {
                *cc += x;
            }
        }
        normalise(&mut centroid);
        let mut ranked: Vec<(usize, f32)> = (0..cands.len())
            .map(|i| (i, correlation(cands[i], &centroid)))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let keep = (ranked.len() * 2 / 3).max(1);
        let typical: Vec<usize> = ranked[..keep].iter().map(|(i, _)| *i).collect();
        set.templates.push(Template {
            class: c,
            v: centroid,
        });
        let mut chosen: Vec<usize> = vec![typical[0]];
        while chosen.len() < per_class.min(typical.len()) {
            let next = typical
                .iter()
                .copied()
                .filter(|i| !chosen.contains(i))
                .min_by(|&a, &b| {
                    let da = chosen
                        .iter()
                        .map(|&k| correlation(cands[a], cands[k]))
                        .fold(f32::MIN, f32::max);
                    let db = chosen
                        .iter()
                        .map(|&k| correlation(cands[b], cands[k]))
                        .fold(f32::MIN, f32::max);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
            match next {
                Some(i) => chosen.push(i),
                None => break,
            }
        }
        for i in chosen {
            set.templates.push(Template {
                class: c,
                v: cands[i].clone(),
            });
        }
        stats.per_class.push((c, cands.len()));
    }
    (set, stats)
}

#[derive(Debug, Default)]
pub struct TrainStats {
    pub used: usize,
    pub skipped_segmentation: usize,
    pub skipped_label: usize,
    /// (class, examples seen)
    pub per_class: Vec<(char, usize)>,
    /// A few frames whose segmentation did not match the label: (label,
    /// boxes as (x, y, w, h)).
    pub mismatches: Vec<(String, Vec<(u32, u32, u32, u32)>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paint a crude glyph: a filled rectangle with an optional hole, enough
    /// to tell classes apart in these tests.
    fn paint(img: &mut GrayImage, x: u32, y: u32, w: u32, h: u32, hole: bool) {
        for yy in y..y + h {
            for xx in x..x + w {
                let inside_hole =
                    hole && xx > x + 2 && xx + 3 < x + w && yy > y + 3 && yy + 4 < y + h;
                if !inside_hole {
                    img.put_pixel(xx, yy, image::Luma([220]));
                }
            }
        }
    }

    #[test]
    fn segments_glyphs_at_column_gaps_and_ignores_a_border() {
        let mut img = GrayImage::from_pixel(200, 60, image::Luma([20]));
        paint(&mut img, 10, 10, 14, 40, true); // "0"-like
        paint(&mut img, 30, 22, 6, 5, false); // colon dot
        paint(&mut img, 30, 36, 6, 5, false);
        paint(&mut img, 40, 10, 14, 40, false); // "1"-like block
        paint(&mut img, 60, 44, 6, 6, false); // "."
        paint(&mut img, 70, 20, 10, 30, true); // small digit
        for yy in 0..60 {
            img.put_pixel(190, yy, image::Luma([255])); // pane border
        }
        let boxes = segment(&img, 60);
        let xs: Vec<u32> = boxes.iter().map(|b| b.x).collect();
        assert_eq!(xs, vec![10, 30, 40, 60, 70], "{boxes:?}");
        assert_eq!(boxes[1].h, 19, "the colon spans both dots");
        assert_eq!(boxes[3].h, 6, "the point is tight");
    }

    #[test]
    fn features_are_brightness_and_scale_invariant() {
        let mut a = GrayImage::from_pixel(60, 60, image::Luma([20]));
        paint(&mut a, 10, 10, 14, 40, true);
        let mut b = GrayImage::from_pixel(120, 120, image::Luma([60]));
        // Same shape, twice the size, dimmer.
        for yy in 20..100 {
            for xx in 20..48 {
                let hole = xx > 24 && xx + 6 < 48 && yy > 26 && yy + 8 < 100;
                if !hole {
                    b.put_pixel(xx, yy, image::Luma([140]));
                }
            }
        }
        let fa = features(
            &a,
            GlyphBox {
                x: 10,
                y: 10,
                w: 14,
                h: 40,
            },
        );
        let fb = features(
            &b,
            GlyphBox {
                x: 20,
                y: 20,
                w: 28,
                h: 80,
            },
        );
        assert!(correlation(&fa, &fb) > 0.95, "{}", correlation(&fa, &fb));
    }

    #[test]
    fn trains_and_reads_a_synthetic_font() {
        // Two classes with distinct shapes: "0" as a hollow block, "1" as a
        // solid one, plus ":" and "." — enough to read "1:01.10".
        let mut corpus: Vec<(GrayImage, String)> = Vec::new();
        for shift in 0..3u32 {
            let mut img = GrayImage::from_pixel(240, 60, image::Luma([15 + shift as u8]));
            let mut x = 8 + shift;
            for c in "1:01.10".chars() {
                match c {
                    '1' => {
                        paint(&mut img, x, 10, 12, 40, false);
                        x += 18;
                    }
                    '0' => {
                        paint(&mut img, x, 10, 14, 40, true);
                        x += 20;
                    }
                    ':' => {
                        paint(&mut img, x, 22, 6, 5, false);
                        paint(&mut img, x, 36, 6, 5, false);
                        x += 12;
                    }
                    '.' => {
                        paint(&mut img, x, 44, 6, 6, false);
                        x += 12;
                    }
                    _ => unreachable!(),
                }
            }
            corpus.push((img, "1:01.10".to_string()));
        }
        let (set, stats) = train(&corpus, 60, 8);
        assert_eq!(stats.used, 3, "{stats:?}");
        assert!(
            set.count('0') >= 1
                && set.count('1') >= 1
                && set.count(':') >= 1
                && set.count('.') >= 1
        );
        let reader = GlyphReader::new(set, 60);
        let boxes = segment(&corpus[1].0, 60);
        let band = band_of(&boxes).unwrap();
        let detail: Vec<String> = boxes
            .iter()
            .map(|b| {
                let f = features(&corpus[1].0, frame_for(*b, band, corpus[1].0.width()));
                format!("{b:?} -> {:?}", classify(&reader.set, &f))
            })
            .collect();
        let r = reader
            .read(&corpus[1].0)
            .unwrap_or_else(|| panic!("readable; glyphs: {detail:#?}"));
        assert_eq!(r.text, "1:01.10");
        assert_eq!(r.boxes.len(), 7);
        // A crop with an unknown shape is declined rather than guessed.
        let mut odd = corpus[0].0.clone();
        paint(&mut odd, 150, 10, 30, 40, false); // a wide blob the font never has
        paint(&mut odd, 165, 20, 4, 4, false);
        assert!(reader.read(&odd).is_none() || reader.read(&odd).unwrap().text != "1:01.10");
    }

    /// A second synthetic font: a narrow solid bar for "1" (like the real
    /// one, narrower than its frame, so its features hold background too)
    /// and a hollow 26x40 block for "0". Draws `c` at `x` and returns where
    /// the next glyph goes after `gap`.
    fn draw_wide(img: &mut GrayImage, x: u32, c: char, gap: u32) -> u32 {
        match c {
            '1' => {
                paint(img, x, 10, 12, 40, false);
                x + 12 + gap
            }
            '0' => {
                paint(img, x, 10, 26, 40, true);
                x + 26 + gap
            }
            ':' => {
                paint(img, x, 22, 6, 5, false);
                paint(img, x, 36, 6, 5, false);
                x + 6 + gap
            }
            '.' => {
                paint(img, x, 44, 6, 6, false);
                x + 6 + gap
            }
            _ => unreachable!(),
        }
    }

    fn border(img: &mut GrayImage, x: u32) {
        for yy in 0..img.height() {
            img.put_pixel(x, yy, image::Luma([255]));
        }
    }

    fn wide_reader() -> GlyphReader {
        let mut corpus: Vec<(GrayImage, String)> = Vec::new();
        for shift in 0..3u32 {
            let mut img = GrayImage::from_pixel(260, 60, image::Luma([15 + shift as u8]));
            let mut x = 8 + shift;
            for c in "1:01.10".chars() {
                x = draw_wide(&mut img, x, c, 6);
            }
            border(&mut img, 250);
            corpus.push((img, "1:01.10".to_string()));
        }
        let (set, stats) = train(&corpus, 60, 8);
        assert_eq!(stats.used, 3, "{stats:?}");
        GlyphReader::new(set, 60)
    }

    /// Real crops the shipped templates must read: the cases that drove the
    /// cut search (a "4" whose crossbar touches the point or the next "4"),
    /// the border's inner edge free-standing and stuck to the last digit,
    /// and two small pairs tesseract misreads ("11" as "14", "77" as "71").
    #[test]
    fn reads_real_crops_with_the_shipped_templates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let reader = GlyphReader::load(&root.join("assets/glyphs.json"), 60).unwrap();
        for (file, want) in [
            ("default-24.29-four-touches-point.png", "24.29"),
            ("default-2.44.44-fours-touch.png", "2:44.44"),
            ("ng-5.09.11-tesseract-said-14.png", "5:09.11"),
            ("ng-2.15.77-tesseract-said-71.png", "2:15.77"),
            ("ng-1.33.21-free-border-sliver.png", "1:33.21"),
            ("ng-42.16-sliver-stuck-to-four.png", "42.16"),
        ] {
            let img = image::open(root.join("tests/fixtures/glyph").join(file))
                .unwrap()
                .to_luma8();
            let r = reader.read_diag(&img);
            assert!(matches!(&r, Ok(rd) if rd.text == want), "{file}: {r:?}");
            let rd = r.unwrap();
            assert_eq!(rd.boxes.len(), want.len(), "{file}: {:?}", rd.boxes);
        }
    }

    #[test]
    fn the_countdown_minus_is_dropped() {
        let reader = wide_reader();
        let mut img = GrayImage::from_pixel(260, 60, image::Luma([15]));
        // A blob mid-band, where no point sits, then "1.10".
        paint(&mut img, 20, 26, 6, 6, false);
        let mut x = 34;
        for c in "1.10".chars() {
            x = draw_wide(&mut img, x, c, 6);
        }
        border(&mut img, 250);
        let r = reader.read_diag(&img);
        assert!(
            matches!(&r, Ok(rd) if rd.text == "1.10"),
            "{r:?}; glyphs {:?}",
            reader.glyphs(&img)
        );
        // The same blob on the baseline is a point, and "..10" is no time.
        let mut img = GrayImage::from_pixel(260, 60, image::Luma([15]));
        paint(&mut img, 20, 44, 6, 6, false);
        let mut x = 34;
        for c in ".10".chars() {
            x = draw_wide(&mut img, x, c, 6);
        }
        border(&mut img, 250);
        assert!(matches!(
            reader.read_diag(&img),
            Err(Decline::Grammar(_)) | Err(Decline::Score(..)) | Err(Decline::Margin(..))
        ));
    }

    #[test]
    fn display_text_matches_livesplit() {
        assert_eq!(display_text(7_660), "7.66");
        assert_eq!(display_text(45_710), "45.71");
        assert_eq!(display_text(116_710), "1:56.71");
        assert_eq!(display_text(695_470), "11:35.47");
        assert_eq!(display_text(3_661_230), "1:01:01.23");
        assert!(time_shaped("1:56.71") && time_shaped("7.66") && time_shaped("1:01:01.23"));
        // Exactly two fraction digits: none or one is a dropped glyph, three
        // a spurious one.
        assert!(!time_shaped("11:35") && !time_shaped("11:35.4") && !time_shaped("1:56.711"));
        assert!(!time_shaped("1:5.71") && !time_shaped(":56"));
    }
}

// ---------------------------------------------------------------------------
// Command line: harvesting templates from a corpus and scoring them.
//
// A corpus directory is what a replay with NG_DUMP_TIMER=all leaves behind:
// `calibration/timer-<frame>.png` for every locked frame, and `obs.jsonl`
// with that frame's reading. A frame labels its glyphs when the tracker
// accepted the reading (it agreed with the running clock within 150 ms) and
// it was read at the primary threshold — the confirmed, unrepaired cases.

#[derive(Debug, Clone)]
pub struct CorpusFrame {
    pub image: GrayImage,
    pub label: String,
    pub layout: String,
}

pub fn load_corpus(dir: &Path, max_frames: usize) -> Result<Vec<CorpusFrame>> {
    let obs = dir.join("obs.jsonl");
    let text =
        std::fs::read_to_string(&obs).with_context(|| format!("reading {}", obs.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(frame), Some(parsed)) = (o["frame"].as_u64(), o["parsed_ms"].as_i64()) else {
            continue;
        };
        if o["retry"].as_u64().is_some() || o["static"].as_bool() == Some(true) {
            continue;
        }
        let Some(smoothed) = o["smoothed_ms"].as_i64() else {
            continue; // not in a run: nothing vouches for the reading
        };
        if (parsed - smoothed).abs() > 150 || parsed < 0 {
            continue;
        }
        // The label is what tesseract READ on this frame, trimmed to the
        // two fraction digits the display has (a third is a phantom) — and
        // it must agree with the value the tracker accepted, so a reading
        // with a phantom in the middle or a dropped digit is not a label.
        let text = o["ocr"].as_str().unwrap_or("").trim().to_string();
        let label = match text.split_once('.') {
            Some((m, f)) if f.chars().count() >= 2 => {
                format!("{m}.{}", f.chars().take(2).collect::<String>())
            }
            _ => text.clone(),
        };
        if label != display_text(parsed) {
            continue;
        }
        let path = dir.join("calibration").join(format!("timer-{frame}.png"));
        let Ok(img) = image::open(&path) else {
            continue;
        };
        out.push(CorpusFrame {
            image: img.to_luma8(),
            label,
            layout: o["layout"].as_str().unwrap_or("?").to_string(),
        });
        if out.len() >= max_frames {
            break;
        }
    }
    Ok(out)
}

/// `glyphs train`: harvest templates from one or more corpus directories.
pub fn cli_train(
    dirs: &[std::path::PathBuf],
    out: &Path,
    level: u8,
    per_class: usize,
) -> Result<()> {
    let mut crops = Vec::new();
    let mut sources = Vec::new();
    for d in dirs {
        let frames = load_corpus(d, 20_000)?;
        println!("{}: {} confirmed frames", d.display(), frames.len());
        for f in &frames {
            if !sources.contains(&f.layout) {
                sources.push(f.layout.clone());
            }
        }
        crops.extend(frames.into_iter().map(|f| (f.image, f.label)));
    }
    if crops.is_empty() {
        bail!("no confirmed frames in the corpus");
    }
    let (mut set, stats) = train(&crops, level, per_class);
    set.sources = sources;
    println!(
        "used {} frames ({} skipped: segmentation did not match the label, {} odd labels)",
        stats.used, stats.skipped_segmentation, stats.skipped_label
    );
    for (c, n) in &stats.per_class {
        println!("  {c:?}: {n} examples -> {} templates", set.count(*c));
    }
    for (label, boxes) in &stats.mismatches {
        println!(
            "  mismatch: {label:?} ({} chars) segmented as {} boxes: {boxes:?}",
            label.len(),
            boxes.len()
        );
    }
    set.save(out)?;
    println!(
        "wrote {} templates to {}",
        set.templates.len(),
        out.display()
    );
    Ok(())
}

/// `glyphs test`: read every confirmed frame of a corpus with the templates
/// and compare with its label.
pub fn cli_test(
    dirs: &[std::path::PathBuf],
    templates: &Path,
    level: u8,
    dump_wrong: Option<&Path>,
) -> Result<()> {
    let reader = GlyphReader::load(templates, level)?;
    let (mut total, mut declined, mut right, mut wrong) = (0usize, 0usize, 0usize, 0usize);
    let mut wrong_examples: Vec<(String, String)> = Vec::new();
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    let mut reason_examples: std::collections::BTreeMap<String, String> = Default::default();
    let (mut right_margins, mut wrong_margins): (Vec<f32>, Vec<f32>) = (Vec::new(), Vec::new());
    if let Some(d) = dump_wrong {
        std::fs::create_dir_all(d)?;
    }
    for d in dirs {
        for f in load_corpus(d, 20_000)? {
            total += 1;
            let result = reader.read_diag(&f.image);
            if let Ok(r) = &result {
                if r.text == f.label {
                    right_margins.push(r.margin);
                } else {
                    wrong_margins.push(r.margin);
                    if let Some(dir) = dump_wrong {
                        let name = format!(
                            "{}__label-{}__read-{}.png",
                            wrong,
                            f.label.replace([':', '.'], "_"),
                            r.text.replace([':', '.'], "_")
                        );
                        let _ = f.image.save(dir.join(name));
                    }
                }
            }
            match result {
                Err(why) => {
                    declined += 1;
                    if let (Some(dir), true) = (dump_wrong, declined <= 24) {
                        let name = format!(
                            "declined-{declined}__label-{}__{}.png",
                            f.label.replace([':', '.'], "_"),
                            match &why {
                                Decline::Score(c, s) => format!("score-{c}-{s:.2}"),
                                Decline::Margin(c, m) => format!("margin-{c}-{m:.2}"),
                                Decline::Grammar(t) =>
                                    format!("grammar-{}", t.replace([':', '.'], "_")),
                                Decline::Segmentation(n) => format!("seg-{n}"),
                            }
                        );
                        let _ = f.image.save(dir.join(name));
                    }
                    let key = match &why {
                        Decline::Segmentation(n) => format!("segmentation ({n} boxes)"),
                        Decline::Score(c, _) => format!("low score on {c:?}"),
                        Decline::Margin(c, _) => format!("thin margin on {c:?}"),
                        Decline::Grammar(_) => "grammar".to_string(),
                    };
                    *reasons.entry(key.clone()).or_default() += 1;
                    reason_examples
                        .entry(key)
                        .or_insert_with(|| format!("{why:?} for label {:?}", f.label));
                }
                Ok(r) if r.text == f.label => right += 1,
                Ok(r) => {
                    wrong += 1;
                    if wrong_examples.len() < 12 {
                        wrong_examples.push((f.label.clone(), r.text));
                    }
                }
            }
        }
    }
    println!(
        "{total} confirmed frames: {right} right ({:.1}%), {declined} declined ({:.1}%), {wrong} WRONG ({:.2}%)",
        100.0 * right as f64 / total.max(1) as f64,
        100.0 * declined as f64 / total.max(1) as f64,
        100.0 * wrong as f64 / total.max(1) as f64,
    );
    let mut sorted: Vec<(String, usize)> = reasons.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, n) in sorted.iter().take(10) {
        println!("  declined, {key}: {n}   e.g. {}", reason_examples[key]);
    }
    for (label, got) in wrong_examples {
        println!("  WRONG: label {label:?} read as {got:?}");
    }
    let pct = |v: &mut Vec<f32>, p: f32| -> f32 {
        if v.is_empty() {
            return f32::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[((v.len() - 1) as f32 * p) as usize]
    };
    println!(
        "  margins — right: p5 {:.3} p25 {:.3} p50 {:.3}; wrong: p50 {:.3} p75 {:.3} p95 {:.3}",
        pct(&mut right_margins, 0.05),
        pct(&mut right_margins, 0.25),
        pct(&mut right_margins, 0.5),
        pct(&mut wrong_margins, 0.5),
        pct(&mut wrong_margins, 0.75),
        pct(&mut wrong_margins, 0.95),
    );
    Ok(())
}

/// `glyphs boxes <png>...`: show how one crop segments and how each glyph
/// scores — the tool for asking "why was this frame declined?".
pub fn cli_boxes(files: &[std::path::PathBuf], templates: &Path, level: u8) -> Result<()> {
    let reader = GlyphReader::load(templates, level)?;
    for f in files {
        let img = image::open(f)
            .with_context(|| format!("opening {}", f.display()))?
            .to_luma8();
        let glyphs = reader.glyphs(&img);
        println!(
            "{}: {}x{}, pane {:?}, band {:?}, {} raw boxes",
            f.display(),
            img.width(),
            img.height(),
            pane_span(&img, level),
            digit_band(&img, level),
            segment(&img, level).len()
        );
        if let Ok((glyphs, band)) = &glyphs {
            for (b, scored) in glyphs {
                let fr = frame_for(*b, *band, img.width());
                match scored {
                    Some((c, s, m)) => println!("  glyph x={:3} y={:2} w={:2} h={:2}  frame x={:3} y={:2} w={:2} h={:2}  -> {c:?} score {s:.2} margin {m:.2}", b.x, b.y, b.w, b.h, fr.x, fr.y, fr.w, fr.h),
                    None => println!("  glyph {b:?}: no class"),
                }
            }
        }
        match reader.read_diag(&img) {
            Ok(r) => println!(
                "  => {:?} (confidence {:.2}, margin {:.2})",
                r.text, r.confidence, r.margin
            ),
            Err(e) => println!("  => declined: {e:?}"),
        }
    }
    Ok(())
}
