//! The learned timer reader: a small convolutional net over digit tiles,
//! trained from this streamer's own VODs by the `timer-ocr` project, whose
//! `net.rs` and `tiles.rs` this file is a copy of (keep them in step). The
//! weights come from `assets/timer_ocr.json` with batch norm already folded
//! in, and everything here is plain arithmetic: no ML runtime.
//!
//! A crop is contrast-normalised, the digits' bounding box is found, and
//! the slots are laid from that box's right edge leftwards, scaled by its
//! height, so the reader is indifferent to where the crop sits, how large
//! the timer is drawn and what colour it is. A crop with no digit band, or
//! one the crop's edge cuts, reads as nothing; a slot read blank between
//! two that were not reads as nothing too. Tesseract (and the glyph
//! reader, when configured) take the frames this declines.

use anyhow::{bail, Context, Result};
use image::imageops::FilterType;
use image::{GrayImage, Luma};
use serde::Deserialize;
use std::path::Path;

/// How a crop is sliced into the tiles the net reads, as the weights
/// carry it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Geometry {
    /// Slots right to left as (offset from the ink's right edge, width),
    /// in pixels at the reference band height.
    pub slots: Vec<[u32; 2]>,
    /// The digit band's height the slots were measured at.
    pub band_ref: u32,
    /// Tile width and height.
    pub tile: [u32; 2],
}

impl Geometry {
    /// How many glyphs a LiveSplit timer prints, right-aligned in these
    /// slots: S.hh and -S.hh, SS.hh, M:SS.hh, MM:SS.hh, H:MM:SS.hh.
    pub const GLYPH_COUNTS: [usize; 5] = [4, 5, 7, 8, 10];

    /// The horizontal scale from the ink's width. The band is a short lever
    /// (35 px on the race total): a pixel or two of it moves the far slot
    /// by half a cell, while the ink is five times as long. The glyph
    /// count is one of a few formats, each spanning a known number of
    /// nominal pixels, so each count implies a scale, and the one nearest
    /// the band's wins. None when no count comes within a fifth of it, or
    /// the slots are fewer than any format.
    pub fn width_scale(&self, bx: &InkBox, band_scale: f32) -> Option<(usize, f32)> {
        let ink_w = (bx.right - bx.left) as f32;
        let first_w = (bx.first_right - bx.left) as f32;
        let last_w = (bx.last_right - bx.last_left) as f32;
        let c_last = self.slots.first()?[1] as f32;
        let mut best: Option<(usize, f32, f32)> = None;
        for k in Self::GLYPH_COUNTS
            .into_iter()
            .filter(|k| *k <= self.slots.len())
        {
            let [from_right, c_first] = self.slots[k - 1];
            // Each end glyph sits centred in its cell, so the ink is the
            // cells' span less half the room left in each end cell:
            // ink = s*span - (s*c_first - first_w)/2 - (s*c_last - last_w)/2.
            let span = (from_right + c_first) as f32 - (c_first as f32 + c_last) / 2.0;
            let s = (ink_w - (first_w + last_w) / 2.0) / span.max(1.0);
            let off = (s / band_scale - 1.0).abs();
            if s > 0.0 && off < 0.2 && best.is_none_or(|b| off < b.2) {
                best = Some((k, s, off));
            }
        }
        best.map(|(k, s, _)| (k, s))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Conv {
    pub out: usize,
    pub inp: usize,
    pub k: usize,
    /// `[out][inp][k][k]`, row-major.
    pub w: Vec<f32>,
    pub b: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fc {
    pub out: usize,
    pub inp: usize,
    /// `[out][inp]`, row-major; the input is the pooled map flattened
    /// channel-major.
    pub w: Vec<f32>,
    pub b: Vec<f32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Weights {
    /// The output order: one character per class (`0-9 : . - _`).
    pub classes: String,
    pub geometry: Geometry,
    pub in_h: usize,
    pub in_w: usize,
    pub conv1: Conv,
    pub conv2: Conv,
    pub fc: Fc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InkBox {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
    /// The rightmost glyph's own columns: the last digit, whose ink is
    /// narrower than its cell when it is a 1.
    pub last_left: u32,
    pub last_right: u32,
    /// Where the leftmost glyph's own columns end (they start at `left`).
    pub first_right: u32,
    /// Where the crop's usable area ends: the crop's edges, or the pane
    /// border's columns where the crop reaches over them.
    pub edge_left: u32,
    pub edge_right: u32,
}

impl InkBox {
    fn height(&self) -> u32 {
        self.bottom - self.top
    }
    fn clipped(&self, _w: u32, h: u32) -> bool {
        // Ink in the first or last usable column, or the first or last row:
        // a glyph that reaches the edge is cut by it.
        self.left <= self.edge_left
            || self.top == 0
            || self.right >= self.edge_right
            || self.bottom >= h
    }
}

/// Stretch the crop so its background is black and its brightest ink
/// white: the 25th percentile is the background (the digits never cover
/// three quarters of the crop) and the 99th percentile the ink. None for
/// a crop with no contrast at all.
pub fn normalise(crop: &GrayImage) -> Option<GrayImage> {
    let mut v: Vec<u8> = crop.as_raw().clone();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    // Which way round is the ink? The median is the background; the ink is
    // whichever tail reaches further from it. Dark digits on a light pane
    // (the race timer) are turned over so that everything after this sees
    // light ink on dark.
    let median = v[v.len() / 2] as f32;
    let lo = v[v.len() / 100] as f32;
    let hi = v[v.len() * 99 / 100] as f32;
    let dark_ink = median - lo > hi - median;
    let (bg, peak) = if dark_ink {
        (255.0 - v[v.len() * 3 / 4] as f32, 255.0 - lo)
    } else {
        (v[v.len() / 4] as f32, hi)
    };
    if peak - bg < 40.0 {
        return None;
    }
    let scale = 255.0 / (peak - bg);
    Some(GrayImage::from_fn(crop.width(), crop.height(), |x, y| {
        let p = crop.get_pixel(x, y).0[0] as f32;
        let p = if dark_ink { 255.0 - p } else { p };
        Luma([((p - bg) * scale).clamp(0.0, 255.0) as u8])
    }))
}

/// The digits' bounding box in a normalised crop: the band of consecutive
/// inked rows with the most ink (so a separator line or the text row under
/// the timer cannot stretch it), and that band's inked columns.
pub fn ink_box(norm: &GrayImage, expect: Option<u32>) -> Option<InkBox> {
    let (w, h) = norm.dimensions();
    let ink = |x: u32, y: u32| norm.get_pixel(x, y).0[0] >= 128;
    // A column inked over nearly the whole crop is the pane's border, not a
    // glyph: the crop ends there. Columns past the first such column on
    // the right (or before the last on the left) are not looked at, and
    // the border is where "cut by the edge" is measured from.
    let full: Vec<bool> = (0..w)
        .map(|x| (0..h).filter(|y| ink(x, *y)).count() as u32 * 10 >= h * 9)
        .collect();
    let x_lo = full
        .iter()
        .rposition(|f| *f)
        .filter(|x| (*x as u32) < w / 2)
        .map(|x| x as u32 + 1)
        .unwrap_or(0);
    let x_hi = full
        .iter()
        .position(|f| *f)
        .filter(|x| (*x as u32) >= w / 2)
        .map(|x| x as u32)
        .unwrap_or(w);
    let ink = |x: u32, y: u32| x >= x_lo && x < x_hi && ink(x, y);
    let mut rows = vec![0u32; h as usize];
    for y in 0..h {
        rows[y as usize] = (0..w).filter(|x| ink(*x, y)).count() as u32;
        // A row inked nearly edge to edge is a bar or a pane's edge, not
        // text (the dark pane above the race timer, turned over).
        if rows[y as usize] * 10 >= (x_hi - x_lo) * 9 {
            rows[y as usize] = 0;
        }
    }
    let mut bands: Vec<(u32, u32, u64)> = Vec::new();
    let mut y = 0;
    while y < h {
        if rows[y as usize] < 2 {
            y += 1;
            continue;
        }
        let top = y;
        let mut sum = 0u64;
        let mut last = y;
        while y < h && (rows[y as usize] >= 2 || (y + 1 < h && rows[(y + 1) as usize] >= 2)) {
            sum += rows[y as usize] as u64;
            if rows[y as usize] >= 2 {
                last = y;
            }
            y += 1;
        }
        bands.push((top, last + 1, sum));
    }
    let (mut top, mut bottom, _) = bands
        .into_iter()
        .filter(|b| b.1 - b.0 >= 8)
        .max_by_key(|b| b.2)?;
    // Two rows of text one above the other touch through their
    // anti-aliasing (the race total and the segment timer under it). Where
    // the band is taller than the digits are known to be (by more than 15%),
    // it is split
    // at its thinnest interior row and the fuller side kept.
    if let Some(e) = expect {
        if (bottom - top) * 20 > e * 23 {
            let (lo, hi) = (top + (bottom - top) / 5, bottom - (bottom - top) / 5);
            if let Some(cut) = (lo..hi).min_by_key(|y| rows[*y as usize]) {
                let above: u64 = (top..cut).map(|y| rows[y as usize] as u64).sum();
                let below: u64 = (cut + 1..bottom).map(|y| rows[y as usize] as u64).sum();
                if above >= below {
                    bottom = cut;
                } else {
                    top = cut + 1;
                }
            }
        }
    }
    // The band's glyphs as runs of inked columns, a run ending at a gap of
    // two empty columns or more (the point and the colon are glyphs of
    // their own).
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut gap = 0;
    for x in 0..w {
        let n = (top..bottom).filter(|y| ink(x, *y)).count();
        if n >= 2 {
            match runs.last_mut() {
                Some(r) if gap < 2 => r.1 = x + 1,
                _ => runs.push((x, x + 1)),
            }
            gap = 0;
        } else {
            gap += 1;
        }
    }
    // A sliver at either edge of the crop is the pane border's anti-aliased
    // inner edge, not a glyph: a few pixels wide where the narrowest glyph
    // (the point) is a tenth of the band.
    let sliver = ((bottom - top) / 10).max(3);
    while runs
        .last()
        .is_some_and(|r| r.1 - r.0 < sliver && r.1 + 3 >= x_hi)
    {
        runs.pop();
    }
    while runs
        .first()
        .is_some_and(|r| r.1 - r.0 < sliver && r.0 <= x_lo + 3)
    {
        runs.remove(0);
    }
    let (left, right) = (runs.first()?.0, runs.last()?.1);
    if right - left < 8 {
        return None;
    }
    let last = runs.last().expect("checked");
    Some(InkBox {
        left,
        right,
        top,
        bottom,
        last_left: last.0,
        last_right: last.1,
        first_right: runs.first().expect("checked").1,
        edge_left: x_lo,
        edge_right: x_hi,
    })
}

/// The tiles of a normalised crop from the slots laid off its ink box;
/// None with no digit band, or one against the crop's edge.
pub fn tiles_ink(norm: &GrayImage, geo: &Geometry) -> Option<(Vec<GrayImage>, InkBox)> {
    let (w, h) = norm.dimensions();
    let bx = ink_box(norm, Some(geo.band_ref))?;
    if bx.clipped(w, h) {
        return None;
    }
    let band_scale = bx.height() as f32 / geo.band_ref.max(1) as f32;
    let scale = geo
        .width_scale(&bx, band_scale)
        .map_or(band_scale, |(_, s)| s);
    // The timer is right-aligned by its cells, not its ink: a last digit
    // narrower than its cell (a 1) leaves the cell's right edge past the
    // ink. The glyph sits centred in the cell, so the cell edge is the
    // glyph's edge plus half the room left over.
    let cell = geo
        .slots
        .first()
        .map(|s| s[1] as f32 * scale)
        .unwrap_or(0.0);
    let glyph_w = (bx.last_right - bx.last_left) as f32;
    let anchor = bx.right as i64 + ((cell - glyph_w).max(0.0) / 2.0).round() as i64;
    let pad = (bx.height() as f32 * 0.1).round() as i64;
    let y0 = (bx.top as i64 - pad).max(0);
    let y1 = (bx.bottom as i64 + pad).min(h as i64);
    let mut out = Vec::with_capacity(geo.slots.len());
    for [from_right, width] in geo.slots.iter().rev() {
        let sw = (*width as f32 * scale).round().max(1.0) as i64;
        let x1 = anchor - (*from_right as f32 * scale).round() as i64;
        let x0 = x1 - sw;
        let mut slot = GrayImage::from_pixel(sw as u32, (y1 - y0) as u32, Luma([0]));
        for sy in y0..y1 {
            for sx in x0.max(0)..x1.min(w as i64) {
                slot.put_pixel(
                    (sx - x0) as u32,
                    (sy - y0) as u32,
                    *norm.get_pixel(sx as u32, sy as u32),
                );
            }
        }
        out.push(image::imageops::resize(
            &slot,
            geo.tile[0],
            geo.tile[1],
            FilterType::Triangle,
        ));
    }
    Some((out, bx))
}

fn softmax_max(logits: &[f32]) -> (usize, f32) {
    let m = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - m).exp()).collect();
    let sum: f32 = exps.iter().sum();
    let (i, e) = exps.iter().enumerate().fold(
        (0, 0.0f32),
        |acc, (i, e)| if *e > acc.1 { (i, *e) } else { acc },
    );
    (i, e / sum)
}

/// k x k convolution with "same" zero padding, bias, ReLU. Input
/// `[cin][h][w]`, output `[cout][h][w]`.
fn conv_relu(x: &[f32], cin: usize, h: usize, w: usize, c: &Conv) -> Vec<f32> {
    let k = c.k;
    let pad = (k / 2) as isize;
    let mut out = vec![0.0f32; c.out * h * w];
    for o in 0..c.out {
        for y in 0..h {
            for xx in 0..w {
                let mut s = c.b[o];
                for i in 0..cin {
                    for ky in 0..k {
                        let yy = y as isize + ky as isize - pad;
                        if yy < 0 || yy >= h as isize {
                            continue;
                        }
                        for kx in 0..k {
                            let xs = xx as isize + kx as isize - pad;
                            if xs < 0 || xs >= w as isize {
                                continue;
                            }
                            s += c.w[((o * cin + i) * k + ky) * k + kx]
                                * x[(i * h + yy as usize) * w + xs as usize];
                        }
                    }
                }
                out[(o * h + y) * w + xx] = s.max(0.0);
            }
        }
    }
    out
}

/// 2x2 max pool, stride 2.
fn maxpool2(x: &[f32], c: usize, h: usize, w: usize) -> (Vec<f32>, usize, usize) {
    let (oh, ow) = (h / 2, w / 2);
    let mut out = vec![0.0f32; c * oh * ow];
    for ch in 0..c {
        for y in 0..oh {
            for xx in 0..ow {
                let mut m = f32::NEG_INFINITY;
                for dy in 0..2 {
                    for dx in 0..2 {
                        m = m.max(x[(ch * h + 2 * y + dy) * w + 2 * xx + dx]);
                    }
                }
                out[(ch * oh + y) * ow + xx] = m;
            }
        }
    }
    (out, oh, ow)
}

impl Weights {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let w: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        w.check()?;
        Ok(w)
    }

    fn check(&self) -> Result<()> {
        for (name, c) in [("conv1", &self.conv1), ("conv2", &self.conv2)] {
            if c.w.len() != c.out * c.inp * c.k * c.k || c.b.len() != c.out {
                bail!("{name}: weight sizes do not match its shape");
            }
        }
        if self.fc.w.len() != self.fc.out * self.fc.inp || self.fc.b.len() != self.fc.out {
            bail!("fc: weight sizes do not match its shape");
        }
        if self.fc.out != self.classes.chars().count() {
            bail!(
                "fc has {} outputs for {} classes",
                self.fc.out,
                self.classes.chars().count()
            );
        }
        if self.conv1.inp != 1
            || self.conv2.inp != self.conv1.out
            || self.fc.inp != self.conv2.out * (self.in_h / 4) * (self.in_w / 4)
        {
            bail!("layer shapes do not chain");
        }
        if self.geometry.tile != [self.in_w as u32, self.in_h as u32]
            || self.geometry.slots.is_empty()
        {
            bail!("geometry does not match the net's input");
        }
        Ok(())
    }

    /// Logits for one tile of `in_h * in_w` bytes.
    fn forward(&self, tile: &[u8]) -> Vec<f32> {
        let (h, w) = (self.in_h, self.in_w);
        debug_assert_eq!(tile.len(), h * w);
        let x: Vec<f32> = tile.iter().map(|p| *p as f32 / 255.0).collect();
        let a = conv_relu(&x, 1, h, w, &self.conv1);
        let (a, h, w) = maxpool2(&a, self.conv1.out, h, w);
        let a = conv_relu(&a, self.conv2.inp, h, w, &self.conv2);
        let (a, _, _) = maxpool2(&a, self.conv2.out, h, w);
        (0..self.fc.out)
            .map(|o| {
                let row = &self.fc.w[o * self.fc.inp..(o + 1) * self.fc.inp];
                row.iter().zip(&a).map(|(w, x)| w * x).sum::<f32>() + self.fc.b[o]
            })
            .collect()
    }

    /// One tile's class and the softmax probability of it.
    pub fn read_tile(&self, tile: &[u8]) -> (char, f32) {
        let (i, p) = softmax_max(&self.forward(tile));
        (self.classes.chars().nth(i).unwrap_or('?'), p)
    }
}

/// Is this a time the way LiveSplit prints one: `S.hh`, `M:SS.hh`,
/// `MM:SS.hh` or `H:MM:SS.hh`, exactly two hundredths, every part after
/// the first exactly two digits? The bot's own parser is forgiving of
/// tesseract's damage; the net's slots are not damaged that way, and a
/// reading outside the print is a slot that slipped.
fn livesplit_print(text: &str) -> bool {
    let Some((whole, frac)) = text.split_once('.') else {
        return false;
    };
    if frac.len() != 2 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let parts: Vec<&str> = whole.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    parts.iter().enumerate().all(|(i, p)| {
        !p.is_empty()
            && p.len() <= 2
            && p.bytes().all(|b| b.is_ascii_digit())
            && (i == 0 || (p.len() == 2 && p.parse::<u32>().is_ok_and(|v| v < 60)))
    })
}

/// What the reader says about a crop.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The timer as printed, the countdown's minus dropped (tesseract and
    /// the glyph reader never report it, and the tracker knows the
    /// countdown by its value).
    pub text: String,
    /// The least confident slot's softmax probability.
    pub confidence: f32,
    /// The digits' box in crop pixels: x, y, w, h.
    pub ink: (u32, u32, u32, u32),
}

pub struct CnnReader {
    weights: Weights,
    /// A reading whose least confident slot is under this is declined.
    min_confidence: f32,
}

impl CnnReader {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            weights: Weights::load(path)?,
            min_confidence: 0.5,
        })
    }

    /// Read a timer crop (light digits on a dark ground, any size).
    pub fn read(&self, crop: &GrayImage) -> Option<Reading> {
        let norm = normalise(crop)?;
        let (tiles, bx) = tiles_ink(&norm, &self.weights.geometry)?;
        let mut text = String::new();
        let mut confidence = 1.0f32;
        let mut seen_ink = false;
        for t in &tiles {
            let (c, p) = self.weights.read_tile(t.as_raw());
            confidence = confidence.min(p);
            if c == '_' {
                if seen_ink {
                    return None;
                }
            } else {
                seen_ink = true;
                text.push(c);
            }
        }
        if confidence < self.min_confidence || text.is_empty() {
            return None;
        }
        let text = text.trim_start_matches('-').to_string();
        // Slots read with confidence that do not spell a time as LiveSplit
        // prints one are another overlay in the crop (a board's cumulative
        // column), or a slot that slipped a cell ("27.391"): not the timer.
        if !livesplit_print(&text) {
            return None;
        }
        crate::timeparse::parse_timer_text(&text)?;
        Some(Reading {
            text,
            confidence,
            ink: (bx.left, bx.top, bx.right - bx.left, bx.bottom - bx.top),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(block: (u32, u32, u32, u32), bright: u8) -> GrayImage {
        let mut img = GrayImage::from_pixel(200, 80, Luma([20]));
        for y in block.1..block.1 + block.3 {
            for x in block.0..block.0 + block.2 {
                img.put_pixel(x, y, Luma([bright]));
            }
        }
        img
    }

    #[test]
    fn the_ink_box_is_the_digits_wherever_they_sit_and_however_dim() {
        for (x, y, b) in [(40, 10, 220u8), (90, 25, 90u8)] {
            let n = normalise(&crop((x, y, 100, 40), b)).unwrap();
            let bx = ink_box(&n, None).unwrap();
            assert_eq!(
                bx,
                InkBox {
                    left: x,
                    right: x + 100,
                    top: y,
                    bottom: y + 40,
                    last_left: x,
                    last_right: x + 100,
                    first_right: x + 100,
                    edge_left: 0,
                    edge_right: 200,
                },
                "{x} {y} {b}"
            );
        }
        assert!(normalise(&GrayImage::from_pixel(50, 20, Luma([30]))).is_none());
    }

    #[test]
    fn only_a_time_as_livesplit_prints_it_is_a_reading() {
        for ok in ["5.00", "34.56", "9:30.77", "12:34.56", "1:02:03.45", "0.20"] {
            assert!(livesplit_print(ok), "{ok}");
        }
        for bad in [
            "27.391", "3:0612", "1:60.00", "1:2.00", "12:34.5", "", "31383400", "5",
        ] {
            assert!(!livesplit_print(bad), "{bad}");
        }
    }

    #[test]
    fn slots_scale_with_the_band_and_a_cut_crop_gives_nothing() {
        let geo = Geometry {
            slots: vec![[0, 20], [20, 20], [40, 20]],
            band_ref: 40,
            tile: [24, 32],
        };
        let n = normalise(&crop((40, 10, 100, 40), 220)).unwrap();
        let (tiles, bx) = tiles_ink(&n, &geo).unwrap();
        assert_eq!(tiles.len(), 3);
        assert!(tiles.iter().all(|t| t.dimensions() == (24, 32)));
        assert_eq!(bx.right, 140);
        let n = normalise(&crop((40, 3, 100, 44), 220)).unwrap();
        assert_eq!(tiles_ink(&n, &geo).unwrap().1.height(), 44);
        let n = normalise(&crop((0, 10, 100, 40), 220)).unwrap();
        assert!(tiles_ink(&n, &geo).is_none());
    }

    /// Real crops from the reference channel against the shipped weights:
    /// the reader as a whole, tile cutting and all.
    #[test]
    fn the_shipped_weights_read_the_fixture_crops() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let reader = CnnReader::load(&root.join("assets/timer_ocr.json")).expect("weights");
        let dir = root.join("tests/fixtures/cnn");
        let mut n = 0;
        let mut wrong = Vec::new();
        for e in std::fs::read_dir(&dir).expect("fixtures").flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "png") {
                continue;
            }
            // The file name is the expected reading with ':' as '-', the
            // point as '_' and the countdown's minus as a leading 'm'
            // ("9-30_77.png" is 9:30.77, "m5_00.png" the countdown at -5.00,
            // read as "5.00" with the sign dropped; "none.png" reads as
            // nothing). Anything after '~' tells duplicates apart.
            let stem = p.file_stem().unwrap().to_str().unwrap();
            let want = stem
                .split('~')
                .next()
                .unwrap()
                .trim_start_matches('m')
                .replace('-', ":")
                .replace('_', ".");
            let img = image::open(&p).unwrap().to_luma8();
            let got = reader.read(&img).map(|r| r.text);
            n += 1;
            let ok = if want == "none" {
                got.is_none()
            } else {
                got.as_deref() == Some(want.as_str())
            };
            if !ok {
                wrong.push(format!("{}: read {:?}", p.display(), got));
            }
        }
        assert!(n >= 10, "only {n} fixture crops");
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Eight glyphs drawn a tenth wider than the band says, as a timer whose
    /// reference band was measured a little tall: the ink's width picks the
    /// eight-glyph format and its scale, and every glyph lands centred in
    /// its tile instead of drifting half a cell by the far end.
    #[test]
    fn the_inks_width_sets_the_horizontal_scale() {
        let geo = Geometry {
            slots: vec![
                [0, 20],
                [20, 20],
                [40, 10],
                [50, 25],
                [75, 25],
                [100, 12],
                [112, 25],
                [137, 25],
            ],
            band_ref: 40,
            tile: [24, 32],
        };
        let sx = 1.1_f32;
        let right = 300.0_f32;
        let mut img = GrayImage::from_pixel(320, 80, Luma([20]));
        for [from_right, width] in &geo.slots {
            let centre = right - (*from_right as f32 + *width as f32 / 2.0) * sx;
            let half = (*width as f32 * sx * 0.3).round();
            for y in 20..60 {
                for x in (centre - half) as u32..(centre + half) as u32 {
                    img.put_pixel(x, y, Luma([220]));
                }
            }
        }
        let n = normalise(&img).unwrap();
        let bx = ink_box(&n, None).unwrap();
        let (k, s) = geo.width_scale(&bx, 1.0).expect("a format fits");
        assert_eq!(k, 8);
        assert!((s - sx).abs() < 0.03, "scale {s}");
        let (tiles, _) = tiles_ink(&n, &geo).unwrap();
        for (i, t) in tiles.iter().enumerate() {
            let cols: Vec<u32> = (0..t.width())
                .filter(|x| (0..t.height()).any(|y| t.get_pixel(*x, y).0[0] > 128))
                .collect();
            let (l, r) = (cols[0], t.width() - 1 - cols[cols.len() - 1]);
            // Within a source pixel and a half: an even block cannot sit centred
            // in an odd slot, and the tile is wider than the slot.
            assert!(
                l.abs_diff(r) <= 3,
                "tile {i}: ink {l} from the left, {r} from the right"
            );
        }
    }
}
