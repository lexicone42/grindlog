//! The geometry-free timer reader: a timer cut into its glyphs by their own
//! ink, with no per-layout slots, and a finder that looks for one in a
//! region nobody configured a crop for.
//!
//! Each glyph is a run of inked columns in the digit band; a run as wide as
//! two digits is two glyphs that touch and is split where the ink is
//! faintest. Each glyph becomes a tile centred on it, three quarters of the
//! band's height wide, so the tile's shape depends on nothing but the digits
//! themselves. The net behind it (`assets/timer_ocr_any.json`) was trained
//! on glyph tiles cut this way from six channels' footage by the sibling
//! `timer-ocr` project, whose `gread.rs` this is a copy of.
//!
//! What it is for: the week after the Big 20 he ran games whose LiveSplit
//! pane fitted none of the configured layouts, and the layout crops read
//! nothing. [`AnyReader::find`] scans the pane for the tallest band that
//! reads as a time, which is where the run timer is on every LiveSplit
//! layout; [`AnyReader::read`] reads a crop the slot reader declined.

use std::path::Path;

use anyhow::Result;
use image::imageops::FilterType;
use image::{GrayImage, Luma};

use crate::cnn::{ink_box, normalise, InkBox, Weights};

/// The glyphs' column runs inside the ink box, touching glyphs split.
pub fn glyph_runs(norm: &GrayImage, bx: &InkBox) -> Vec<(u32, u32)> {
    let ink = |x: u32, y: u32| norm.get_pixel(x, y).0[0] >= 128;
    let cols: Vec<u32> = (0..norm.width())
        .map(|x| (bx.top..bx.bottom).filter(|y| ink(x, *y)).count() as u32)
        .collect();
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut gap = 0;
    for x in bx.left..bx.right {
        if cols[x as usize] >= 2 {
            match runs.last_mut() {
                Some(r) if gap < 2 => r.1 = x + 1,
                _ => runs.push((x, x + 1)),
            }
            gap = 0;
        } else {
            gap += 1;
        }
    }
    let h = (bx.bottom - bx.top) as f32;
    // A run's own height: the rows its columns ink.
    let height = |a: u32, b: u32| {
        let rows: Vec<u32> = (bx.top..bx.bottom)
            .filter(|y| (a..b).any(|x| ink(x, *y)))
            .collect();
        rows.last().map_or(0, |l| l + 1 - rows[0])
    };
    // A small hollow glyph (a hundredths 0 or 8 at 480p) can come apart
    // into its two strokes: its middle columns hold almost no ink. Two
    // tall, narrow runs (each a stroke, under a third of the band wide; a
    // whole digit is four tenths) close together (within 0.15 of the band)
    // that together are no wider than a digit are one.
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (a, b) in runs {
        if let Some(last) = merged.last_mut() {
            let gap = a.saturating_sub(last.1) as f32;
            let tall = |x: u32, y: u32| height(x, y) as f32 >= 0.4 * h;
            let narrow = |x: u32, y: u32| ((y - x) as f32) <= 0.3 * h;
            if gap <= 0.15 * h
                && (b - last.0) as f32 <= 0.75 * h
                && narrow(last.0, last.1)
                && narrow(a, b)
                && tall(last.0, last.1)
                && tall(a, b)
            {
                last.1 = b;
                continue;
            }
        }
        merged.push((a, b));
    }
    let runs = merged;
    let mut out = Vec::new();
    for (a, b) in runs {
        let w = (b - a) as f32;
        // One digit is taller than it is wide, whatever its size (a
        // timer's hundredths are drawn smaller than its seconds, so a width
        // learned from the others splits the big ones); two that touch are
        // wider than tall. A short wide mark (the countdown's minus) is
        // one glyph.
        let rh = height(a, b) as f32;
        let digit = 0.65 * rh;
        let k = (w / digit).round() as u32;
        if rh < 0.5 * h || w < 1.1 * rh || k < 2 {
            out.push((a, b));
            continue;
        }
        // k glyphs: cut near each expected boundary at the faintest column.
        let mut start = a;
        for i in 1..k {
            let expect = a as f32 + w * i as f32 / k as f32;
            let reach = (digit * 0.25).max(1.0);
            let lo = (expect - reach).max(start as f32 + 1.0) as u32;
            let hi = ((expect + reach) as u32).min(b - 1);
            let cut = (lo..=hi.max(lo))
                .min_by_key(|x| cols[*x as usize])
                .unwrap_or(lo);
            out.push((start, cut));
            start = cut;
        }
        out.push((start, b));
    }
    // A last run a quarter of the band tall or less is not the last glyph
    // (a hundredths digit is over half of it) but a fragment of the text
    // under the timer reaching into the band: dropped. Only at the end:
    // the countdown's minus, as short, always comes first.
    while out.len() > 1
        && out
            .last()
            .is_some_and(|(a, b)| (height(*a, *b) as f32) <= 0.25 * h)
    {
        out.pop();
    }
    out
}

/// A tile per glyph: centred on it, the band plus a tenth above and below,
/// three quarters of that height wide, resized to the net's tile.
pub fn glyph_tiles(
    norm: &GrayImage,
    bx: &InkBox,
    runs: &[(u32, u32)],
    tile: [u32; 2],
) -> Vec<GrayImage> {
    let (w, h) = norm.dimensions();
    let band = (bx.bottom - bx.top) as f32;
    let pad = (band * 0.1).round() as i64;
    let y0 = bx.top as i64 - pad;
    let y1 = bx.bottom as i64 + pad;
    let cw = ((y1 - y0) as f32 * tile[0] as f32 / tile[1] as f32)
        .round()
        .max(1.0) as i64;
    runs.iter()
        .map(|(a, b)| {
            let cx = (*a as i64 + *b as i64) / 2;
            let x0 = cx - cw / 2;
            let mut cell = GrayImage::from_pixel(cw as u32, (y1 - y0) as u32, Luma([0]));
            for sy in y0.max(0)..y1.min(h as i64) {
                for sx in x0.max(0)..(x0 + cw).min(w as i64) {
                    cell.put_pixel(
                        (sx - x0) as u32,
                        (sy - y0) as u32,
                        *norm.get_pixel(sx as u32, sy as u32),
                    );
                }
            }
            image::imageops::resize(&cell, tile[0], tile[1], FilterType::Triangle)
        })
        .collect()
}

/// The digit band of a normalised crop, or None when the band is cut by
/// the crop's edge.
pub fn band(norm: &GrayImage, expect: Option<u32>) -> Option<InkBox> {
    let (w, h) = norm.dimensions();
    let mut bx = ink_box(norm, expect)?;
    // With no layout to say how tall the digits are, a band that runs to
    // the crop's top or bottom is two rows of text that touch (a race
    // total and the segment timer under it): try again expecting the
    // larger row, about six tenths of the merged band, and let the split
    // separate them.
    if expect.is_none() && (bx.top == 0 || bx.bottom >= h || bx.clipped(w, h)) {
        if let Some(b) = ink_box(norm, Some((bx.height() as f32 * 0.6) as u32)) {
            bx = b;
        }
    }
    (!bx.clipped(w, h)).then_some(bx)
}

/// A crop's glyph tiles and its ink box, or None when there is no digit
/// band or it is cut.
pub fn tiles(crop: &GrayImage, tile: [u32; 2]) -> Option<(Vec<GrayImage>, InkBox)> {
    let norm = normalise(crop)?;
    let bx = band(&norm, None)?;
    let runs = glyph_runs(&norm, &bx);
    if runs.is_empty() {
        return None;
    }
    Some((glyph_tiles(&norm, &bx, &runs, tile), bx))
}

/// A time as LiveSplit prints it, strictly: an optional minus, one to
/// three colon parts of at most two digits (the later ones exactly two and
/// under sixty), and exactly two fraction digits, or none at all for a timer
/// set to whole seconds. The looser `timeparse::parse_time` repairs
/// tesseract's damage; glyphs read by a net need no repair, and a reading
/// that lost its point ("27391") must not become a time.
pub fn parse_print(text: &str) -> Option<i64> {
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (whole, frac) = match t.split_once('.') {
        Some((w, f)) => {
            if f.len() != 2 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (w, f.parse::<i64>().ok()? * 10)
        }
        None => (t, 0),
    };
    let parts: Vec<&str> = whole.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut secs: i64 = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 2 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if i > 0 && (p.len() != 2 || p.parse::<i64>().ok()? >= 60) {
            return None;
        }
        secs = secs * 60 + p.parse::<i64>().ok()?;
    }
    let ms = secs * 1000 + frac;
    Some(if neg { -ms } else { ms })
}

/// A timer read geometry-free.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The timer as printed, a countdown's minus dropped.
    pub text: String,
    /// The least confident glyph's softmax probability.
    pub confidence: f32,
    /// The digits' box in the crop's pixels: x, y, w, h.
    pub ink: (u32, u32, u32, u32),
}

/// A timer found in a region: where its digits are, in the region's
/// pixels, and what they read.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub ink: (u32, u32, u32, u32),
    pub text: String,
    pub ms: i64,
}

/// The reader with its net.
pub struct AnyReader {
    weights: Weights,
    min_confidence: f32,
}

/// A band under this many rows is not a timer anyone reads a run off.
const MIN_BAND: u32 = 16;
/// Nor one over this many: the band is the whole pane.
const MAX_BAND: u32 = 160;

impl AnyReader {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            weights: Weights::load(path)?,
            min_confidence: 0.5,
        })
    }

    fn tile(&self) -> [u32; 2] {
        [self.weights.in_w as u32, self.weights.in_h as u32]
    }

    /// Read a timer crop (light digits on a dark ground, any size, the
    /// digits anywhere in it). None when the crop holds no digit band, a
    /// glyph is read with too little confidence, or the glyphs do not
    /// spell a time as LiveSplit prints one.
    pub fn read(&self, crop: &GrayImage) -> Option<Reading> {
        let (tiles, bx) = tiles(crop, self.tile())?;
        let h = bx.height();
        if !(MIN_BAND..=MAX_BAND).contains(&h) {
            return None;
        }
        let mut text = String::new();
        let mut confidence = 1.0f32;
        for t in &tiles {
            let (c, p) = self.weights.read_tile(t.as_raw());
            confidence = confidence.min(p);
            if c != '_' {
                text.push(c);
            }
        }
        if confidence < self.min_confidence || text.is_empty() {
            return None;
        }
        parse_print(&text)?;
        let text = text.trim_start_matches('-').to_string();
        Some(Reading {
            text,
            confidence,
            ink: (bx.left, bx.top, bx.right - bx.left, h),
        })
    }

    /// Is there a digit band in this crop at all? What tesseract is spared
    /// where there is not.
    pub fn has_band(&self, crop: &GrayImage) -> bool {
        tiles(crop, self.tile()).is_some_and(|(_, bx)| (MIN_BAND..=MAX_BAND).contains(&bx.height()))
    }

    /// Find the run timer in a region: the tallest digit band that reads
    /// as a time. The region is scanned in strips a little over the
    /// tallest band high, half a strip apart, so a band sits whole in one
    /// of them; each strip's band is read, and of the strips that read a
    /// time the tallest band wins (a split row's time cell and the segment
    /// timer under the run's are both smaller). Two strips reading the
    /// same band agree on its box, so the tallest is found once.
    pub fn find(&self, region: &GrayImage) -> Option<Found> {
        let (w, h) = region.dimensions();
        let strip = (MAX_BAND + MAX_BAND / 4).min(h);
        let step = (strip / 2).max(1);
        let mut best: Option<Found> = None;
        let mut y = 0u32;
        loop {
            let sh = strip.min(h - y);
            let crop = image::imageops::crop_imm(region, 0, y, w, sh).to_image();
            if let Some(r) = self.read(&crop) {
                if let Some(ms) = parse_print(&r.text).filter(|m| *m >= 0) {
                    let ink = (r.ink.0, r.ink.1 + y, r.ink.2, r.ink.3);
                    if best.as_ref().is_none_or(|b| ink.3 > b.ink.3) {
                        best = Some(Found {
                            ink,
                            text: r.text,
                            ms,
                        });
                    }
                }
            }
            if y + sh >= h {
                break;
            }
            y += step;
        }
        best
    }
}

/// The crop to read a timer from once it is found: room for the hours to
/// come (two bands to the left), a quarter band to the right, half a band
/// above and below, clamped to the region.
pub fn crop_around(ink: (u32, u32, u32, u32), region: (u32, u32)) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = ink;
    let x0 = x.saturating_sub(2 * h);
    let y0 = y.saturating_sub(h / 2);
    let x1 = (x + w + h / 4).min(region.0);
    let y1 = (y + h + h / 2).min(region.1);
    (
        x0,
        y0,
        x1.saturating_sub(x0).max(1),
        y1.saturating_sub(y0).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: u32, h: u32, strokes: &[(u32, u32, u32, u32)]) -> GrayImage {
        let mut img = GrayImage::from_pixel(w, h, Luma([20]));
        for (x0, x1, y0, y1) in strokes {
            for y in *y0..*y1 {
                for x in *x0..*x1 {
                    img.put_pixel(x, y, Luma([220]));
                }
            }
        }
        img
    }

    /// A small hollow glyph whose middle columns came out empty (two thin
    /// strokes 2 px apart) is one glyph; two whole digits the same
    /// distance apart are two.
    #[test]
    fn the_strokes_of_a_hollow_glyph_are_one_glyph() {
        let img = canvas(
            200,
            60,
            &[
                (40, 46, 10, 50),
                (48, 54, 10, 50),
                (100, 116, 10, 50),
                (118, 134, 10, 50),
            ],
        );
        let norm = normalise(&img).unwrap();
        let bx = ink_box(&norm, None).unwrap();
        assert_eq!(
            glyph_runs(&norm, &bx),
            vec![(40, 54), (100, 116), (118, 134)]
        );
    }

    /// Two digits that touch are cut at the faintest column between them.
    #[test]
    fn touching_glyphs_are_cut_at_their_seam() {
        // Two 26-wide digits touching at x=66, with a faint column there.
        let mut img = canvas(200, 60, &[(40, 92, 10, 50)]);
        for y in 10..50 {
            img.put_pixel(66, y, Luma([20]));
            img.put_pixel(66, 10, Luma([220]));
            img.put_pixel(66, 49, Luma([220]));
        }
        let norm = normalise(&img).unwrap();
        let bx = ink_box(&norm, None).unwrap();
        let runs = glyph_runs(&norm, &bx);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!((runs[0].1 as i32 - 66).abs() <= 2, "{runs:?}");
    }

    /// The print is strict: LiveSplit's forms and nothing else.
    #[test]
    fn a_print_is_read_strictly() {
        assert_eq!(parse_print("1:02:03.45"), Some(3_723_450));
        assert_eq!(parse_print("12:34.56"), Some(754_560));
        assert_eq!(parse_print("5.00"), Some(5_000));
        assert_eq!(parse_print("-5.00"), Some(-5_000));
        assert_eq!(parse_print("15:27"), Some(927_000));
        assert_eq!(parse_print("0:27:05"), Some(1_625_000));
        for bad in [
            "27391",
            "1:05.3",
            "12.345",
            "1:60.00",
            "100:00.00",
            "",
            ":",
            "1:5.00",
        ] {
            assert_eq!(parse_print(bad), None, "{bad:?}");
        }
    }

    /// The crop around a found timer leaves room for the hours.
    #[test]
    fn the_crop_around_a_timer_leaves_room_to_the_left() {
        assert_eq!(
            crop_around((300, 500, 200, 40), (800, 700)),
            (220, 480, 290, 80)
        );
        assert_eq!(crop_around((10, 10, 100, 40), (800, 700)), (0, 0, 120, 70));
    }
}

#[cfg(test)]
mod pane_fixtures {
    use super::*;

    fn reader() -> AnyReader {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        AnyReader::load(&root.join("assets/timer_ocr_any.json")).expect("the shipped weights")
    }

    fn pane(name: &str) -> GrayImage {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        image::open(root.join("tests/fixtures/pane").join(name))
            .expect("the fixture")
            .to_luma8()
    }

    /// The capture's union crop of the two panes no layout fitted, an hour
    /// and a half into each broadcast: the finder reads the run timer off
    /// each, the tallest band in the pane, and nothing else.
    #[test]
    fn the_finder_reads_the_run_timer_off_a_pane_no_layout_fits() {
        let r = reader();
        let cry = r
            .find(&pane("crystalis-2026-10-16.png"))
            .expect("Crystalis's timer");
        assert_eq!(cry.text, "1:28:47.83", "{cry:?}");
        let dw2 = r
            .find(&pane("dragon-warrior-ii-2026-10-14.png"))
            .expect("Dragon Warrior II's timer");
        assert_eq!(dw2.text, "1:29:26.63", "{dw2:?}");
        // And the crop around it reads the same, as the run loop will.
        let c = crop_around(cry.ink, (688, 660));
        let crop = image::imageops::crop_imm(&pane("crystalis-2026-10-16.png"), c.0, c.1, c.2, c.3)
            .to_image();
        assert_eq!(r.read(&crop).map(|x| x.text).as_deref(), Some("1:28:47.83"));
    }
}
