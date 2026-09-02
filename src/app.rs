//! The main run mode: capture -> preprocess -> OCR -> state machine -> DB/chat.

use anyhow::{Context, Result};
use image::{GrayImage, Luma, RgbImage};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::db::{self, NewRun};
use crate::ocr::{self, OcrEngine, PreprocessCfg};
use crate::state::{Event, Obs, Tracker};
use crate::timeparse::{format_ms, parse_time};
use crate::{capture, chat, util};

/// State shared with the chat task.
pub struct Shared {
    /// (game, category) currently being tracked.
    pub game: RwLock<(String, String)>,
    pub status: RwLock<Status>,
    /// Act boundaries for the death chart (name, cumulative end_ms).
    pub acts: Vec<(String, Option<i64>)>,
    /// Splits recorded so far in the run currently in progress.
    pub current_splits: RwLock<Vec<crate::splits::RecordedSplit>>,
    /// What the tracked best is called ("PB", "season best", ...).
    pub record_label: String,
    /// Best known time from before tracking started; a "NEW record"
    /// announcement must beat this too.
    pub baseline_best_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub phase: String,
    pub smoothed_ms: Option<i64>,
    /// ms since a reading was last actually accepted; large = smoothed_ms is
    /// a projection (death screen, menu, ad break), not an observation.
    pub read_age_ms: Option<i64>,
    pub last_ocr: Option<String>,
    pub updated_unix_ms: i64,
    /// Share of this session's frames whose timer OCR parsed.
    pub parse_pct: Option<f64>,
    /// Locked layout and pixel offset, or "probing".
    pub layout: String,
}

struct CurrentRun {
    game: String,
    category: String,
    attempt_number: i64,
    started_unix_ms: i64,
    session_id: Option<i64>,
    ls_attempt: Option<i64>,
    splits: Vec<crate::splits::RecordedSplit>,
}

/// The ffmpeg-side crop and the sub-rectangles inside it. When splits OCR is
/// enabled, ffmpeg delivers the union bounding box of both regions and we
/// sub-crop in Rust; otherwise the union IS the timer crop.
#[derive(Debug, Clone)]
pub struct Regions {
    pub union: (u32, u32, u32, u32), // x, y, w, h in canvas coords
    pub timer: (u32, u32, u32, u32), // relative to union
    pub splits: Option<(u32, u32, u32, u32)>, // relative to union
    pub counter: Option<(u32, u32, u32, u32)>, // relative to union
    pub sob: Option<(u32, u32, u32, u32)>, // relative to union
}

type R = (u32, u32, u32, u32);

/// Absolute canvas rectangles for one layout: layout 0 is the base config
/// sections; alternates override rectangles they specify and inherit the rest.
pub(crate) struct LayoutRects {
    pub(crate) name: String,
    pub(crate) timer: R,
    pub(crate) splits: Option<R>,
    pub(crate) counter: Option<R>,
    pub(crate) sob: Option<R>,
}

pub(crate) fn layout_rects(cfg: &Config) -> Vec<LayoutRects> {
    let t = &cfg.timer;
    let s = &cfg.splits;
    let c = &cfg.attempts_counter;
    let b = &cfg.lifetime_sob;
    let base = LayoutRects {
        name: "default".into(),
        timer: (t.crop_x, t.crop_y, t.crop_w, t.crop_h),
        splits: s
            .enabled
            .then_some((s.crop_x, s.crop_y, s.crop_w, s.crop_h)),
        counter: c
            .enabled
            .then_some((c.crop_x, c.crop_y, c.crop_w, c.crop_h)),
        sob: b
            .enabled
            .then_some((b.crop_x, b.crop_y, b.crop_w, b.crop_h)),
    };
    let rect = |r: crate::config::Rect| (r.crop_x, r.crop_y, r.crop_w, r.crop_h);
    let mut all = vec![base];
    for l in &cfg.layouts {
        let base = &all[0];
        all.push(LayoutRects {
            name: l.name.clone(),
            timer: rect(l.timer),
            splits: base.splits.map(|d| l.splits.map(rect).unwrap_or(d)),
            counter: base
                .counter
                .map(|d| l.attempts_counter.map(rect).unwrap_or(d)),
            sob: base.sob.map(|d| l.lifetime_sob.map(rect).unwrap_or(d)),
        });
    }
    all
}

/// One `Regions` per layout, all relative to a single union crop that covers
/// every layout — ffmpeg delivers that union once per frame.
pub fn regions(cfg: &Config) -> Vec<Regions> {
    let layouts = layout_rects(cfg);
    let rects: Vec<R> = layouts
        .iter()
        .flat_map(|l| {
            std::iter::once(l.timer)
                .chain(l.splits)
                .chain(l.counter)
                .chain(l.sob)
        })
        .collect();
    // Pad the union by the drift-search margin plus one grid step (clamped to
    // the canvas) so offset probes, and the fine correction that follows a
    // grid lock, stay inside the decoded frame.
    let pad = cfg.layout_search.drift_px + cfg.layout_search.step_px;
    let (cw, chh) = (cfg.stream.canvas_w, cfg.stream.canvas_h);
    let ux = rects.iter().map(|r| r.0).min().unwrap().saturating_sub(pad);
    let uy = rects.iter().map(|r| r.1).min().unwrap().saturating_sub(pad);
    let uw = (rects.iter().map(|r| r.0 + r.2).max().unwrap() + pad).min(cw) - ux;
    let uh = (rects.iter().map(|r| r.1 + r.3).max().unwrap() + pad).min(chh) - uy;
    let rel = |r: R| (r.0 - ux, r.1 - uy, r.2, r.3);
    layouts
        .iter()
        .map(|l| Regions {
            union: (ux, uy, uw, uh),
            timer: rel(l.timer),
            splits: l.splits.map(rel),
            counter: l.counter.map(rel),
            sob: l.sob.map(rel),
        })
        .collect()
}

/// Names of the configured layouts, index-aligned with `regions()`.
pub fn layout_names(cfg: &Config) -> Vec<String> {
    layout_rects(cfg).into_iter().map(|l| l.name).collect()
}

/// Luma grayscale, as ffmpeg's "gray" output would be: LiveSplit's blue
/// highlight row stays dark, so the splits column and the pane analysis
/// read white-on-blue text.
fn to_luma(rgb: &RgbImage) -> GrayImage {
    image::imageops::grayscale(rgb)
}

/// Brightest-channel grayscale for the timer, counter and Sum of Best: a red
/// "behind pace" timer is pure red, which luma turns into a dim ~76/255 that
/// erodes glyphs (a 7 reading as 1) or drops under the threshold entirely.
/// The brightest channel keeps every LiveSplit timer colour at full strength.
fn to_bright(rgb: &RgbImage) -> GrayImage {
    let (w, h) = rgb.dimensions();
    GrayImage::from_fn(w, h, |x, y| {
        let p = rgb.get_pixel(x, y).0;
        Luma([p[0].max(p[1]).max(p[2])])
    })
}

/// Did anything inside `rect` change between two frames? Compression noise
/// nudges pixels by a few levels; a digit changing moves hundreds of pixels
/// by a lot. Counts pixels that moved more than 40 levels and calls the
/// region changed once they exceed 0.2% of its area (at least 40 pixels).
/// Every tesseract call costs ~120ms of process startup before it reads a
/// single glyph, so OCR-ing a crop that is pixel-for-pixel the same as the
/// last one is the most expensive way to learn nothing.
fn region_changed(prev: &GrayImage, cur: &GrayImage, (x, y, w, h): R) -> bool {
    if prev.dimensions() != cur.dimensions() {
        return true;
    }
    let limit = ((w as usize * h as usize) / 1000).max(20);
    let mut moved = 0usize;
    for yy in y..y + h {
        for xx in x..x + w {
            let a = prev.get_pixel(xx, yy).0[0] as i16;
            let b = cur.get_pixel(xx, yy).0[0] as i16;
            if (a - b).abs() > 30 {
                moved += 1;
                if moved > limit {
                    return true;
                }
            }
        }
    }
    false
}

/// Every rectangle of a layout moved by a pixel offset (the LiveSplit window
/// nudged), or None if any of them would leave the union crop.
fn shifted(reg: &Regions, (dx, dy): (i32, i32)) -> Option<Regions> {
    let (_, _, uw, uh) = reg.union;
    let sh = |r: R| -> Option<R> {
        let x = r.0 as i64 + dx as i64;
        let y = r.1 as i64 + dy as i64;
        let inside = x >= 0 && y >= 0 && x + r.2 as i64 <= uw as i64 && y + r.3 as i64 <= uh as i64;
        inside.then_some((x as u32, y as u32, r.2, r.3))
    };
    let opt = |r: Option<R>| -> Option<Option<R>> {
        match r {
            Some(r) => sh(r).map(Some),
            None => Some(None),
        }
    };
    Some(Regions {
        union: reg.union,
        timer: sh(reg.timer)?,
        splits: opt(reg.splits)?,
        counter: opt(reg.counter)?,
        sob: opt(reg.sob)?,
    })
}

/// Offsets to probe around each layout, nearest first, origin excluded.
fn drift_offsets(cfg: &crate::config::LayoutSearchCfg) -> Vec<(i32, i32)> {
    let max = cfg.drift_px as i32;
    let step = cfg.step_px.max(1) as i32;
    let axis: Vec<i32> = (-(max / step)..=(max / step)).map(|k| k * step).collect();
    let mut offs: Vec<(i32, i32)> = axis
        .iter()
        .flat_map(|&dx| axis.iter().map(move |&dy| (dx, dy)))
        .filter(|&o| o != (0, 0))
        .collect();
    offs.sort_by_key(|&(dx, dy)| (dx.abs().max(dy.abs()), dx * dx + dy * dy));
    offs
}

/// Where the digits sit inside a processed (ink = black) timer crop: the
/// ink's right edge and vertical centre, in un-upscaled crop pixels.
/// LiveSplit right-aligns the timer, so both stay put while the window does;
/// a consistent shift means the window moved.
///
/// A crop often reaches the pane's border to the right of the digits — ink
/// spanning the full crop height, which tesseract folds into the word box
/// too. Starting from where the text begins (`text_left`, from tesseract),
/// the first full-height column is that border, and only columns left of it
/// count.
fn ink_anchor(proc: &GrayImage, text_left: Option<u32>, upscale: u32) -> Option<(i32, i32)> {
    let up = upscale.max(1);
    let (w, h) = proc.dimensions();
    let mut cols = vec![0u32; w as usize];
    for (x, _, p) in proc.enumerate_pixels() {
        if p.0[0] == 0 {
            cols[x as usize] += 1;
        }
    }
    let start = text_left.unwrap_or(0).min(w) as usize;
    let border = cols[start..]
        .iter()
        .position(|&c| c >= h * 95 / 100)
        .map(|i| start + i);
    let end = border.map_or(w as usize, |b| b.saturating_sub(3 * up as usize));
    let min = 3 * up;
    let digit_col = |x: usize| x >= start && x < end && cols[x] >= min && cols[x] <= h * 9 / 10;
    let right = (start..end).rev().find(|&x| digit_col(x))? as f32;
    let mut rows = vec![0u32; h as usize];
    for (x, y, p) in proc.enumerate_pixels() {
        if p.0[0] == 0 && digit_col(x as usize) {
            rows[y as usize] += 1;
        }
    }
    let top = rows.iter().position(|&c| c >= min)? as f32;
    let bottom = rows.iter().rposition(|&c| c >= min)? as f32;
    let up = up as f32;
    Some((
        ((right + 1.0) / up).round() as i32,
        ((top + bottom) / 2.0 / up).round() as i32,
    ))
}

/// Pane geometry measured from the frame itself, relative to the timer
/// rectangle's top-left corner: where the splits column and the attempt
/// counter really are. The streamer resizes the LiveSplit window between
/// days, which changes the row pitch — something no translation of the
/// configured rectangles can follow.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PaneGeometry {
    /// Splits column: (dx, dy, w, h) from the timer's top-left.
    splits: (i32, i32, u32, u32),
    /// Attempt counter, same convention.
    counter: Option<(i32, i32, u32, u32)>,
    /// "Sum of Best" row's time, same convention.
    sob: Option<(i32, i32, u32, u32)>,
    rows_read: usize,
    pitch: u32,
}

impl PaneGeometry {
    /// Rebuild `regs`' splits/counter rectangles from the geometry (union
    /// coordinates, clamped to the union).
    fn apply(&self, regs: &mut Regions) {
        let (_, _, uw, uh) = regs.union;
        let place = |(dx, dy, w, h): (i32, i32, u32, u32)| -> Option<R> {
            let x = regs.timer.0 as i64 + dx as i64;
            let y = regs.timer.1 as i64 + dy as i64;
            let inside = x >= 0 && y >= 0 && x + w as i64 <= uw as i64 && y + h as i64 <= uh as i64;
            inside.then_some((x as u32, y as u32, w, h))
        };
        if let Some(r) = place(self.splits) {
            regs.splits = Some(r);
        }
        if let Some(r) = self.counter.and_then(place) {
            regs.counter = Some(r);
        }
        if let Some(r) = self.sob.and_then(place) {
            regs.sob = Some(r);
        }
    }
}

fn time_shaped(text: &str) -> bool {
    let t = text.trim().trim_end_matches('.');
    t.contains([':', '.']) && parse_time(t).is_some()
}

/// Derive the pane geometry from the split rows visible above the timer:
/// time-shaped words grouped into rows, rightmost word per row (LiveSplit's
/// cumulative column), median row pitch; the column block is anchored at the
/// lowest row and spans `acts` rows. A bare integer above the block is the
/// attempt counter. Below the timer, a time whose row label (from the
/// `letters` pass) says "Sum of Best" is the SoB row. None when fewer than
/// two rows read or the pitch is implausible.
fn pane_geometry(
    words: &[ocr::Word],
    letters: &[ocr::Word],
    scale: u32,
    timer: R,
    acts: u32,
) -> Option<PaneGeometry> {
    let sc = scale.max(1);
    let band_x0 = timer.0.saturating_sub(timer.2 / 2) as i64;
    let band_x1 = (timer.0 + timer.2 + timer.2 / 2) as i64;
    let boxes: Vec<(R, &ocr::Word)> = words
        .iter()
        .filter(|w| w.conf >= 30.0)
        .map(|w| ((w.x / sc, w.y / sc, w.w.max(1) / sc, w.h.max(1) / sc), w))
        .collect();
    let cy = |r: R| r.1 as i64 + r.3 as i64 / 2;
    let mut above: Vec<R> = boxes
        .iter()
        .filter(|(r, w)| {
            time_shaped(&w.text)
                && r.1 + r.3 <= timer.1 + 4
                && (r.0 + r.2) as i64 > band_x0
                && (r.0 as i64) < band_x1
                && r.3 >= 10
                && r.3 < timer.3
        })
        .map(|(r, _)| *r)
        .collect();
    above.sort_by_key(|r| cy(*r));
    let mut rows: Vec<Vec<R>> = Vec::new();
    for r in above {
        match rows.last_mut() {
            Some(row) if (cy(row[0]) - cy(r)).abs() <= (r.3 as i64 / 2).max(4) => row.push(r),
            _ => rows.push(vec![r]),
        }
    }
    let col: Vec<R> = rows
        .iter()
        .map(|row| *row.iter().max_by_key(|r| r.0 + r.2).unwrap())
        .collect();
    if col.len() < 2 {
        return None;
    }
    let mut gaps: Vec<i64> = col.windows(2).map(|w| cy(w[1]) - cy(w[0])).collect();
    gaps.sort_unstable();
    let pitch = gaps[gaps.len() / 2];
    if !(24..=80).contains(&pitch) {
        return None;
    }
    // The cumulative column is right-aligned: its x-extent comes from the
    // words that share the rightmost edge, so a row where only the segment
    // time was read cannot widen the column leftwards.
    let right_edge = col.iter().map(|r| r.0 + r.2).max()? as i64;
    let aligned: Vec<R> = col
        .iter()
        .filter(|r| right_edge - (r.0 + r.2) as i64 <= 12)
        .copied()
        .collect();
    // Room for one more digit on the left than the widest value read: the
    // rows read at lock time may all be sub-10-minute, and "11:35.1" is a
    // digit wider than "8:38.6" — clipped, it reads as "1:35.1".
    // Two digits of headroom: sparse-mode OCR drops a glyph that touches the
    // crop edge, and the rows read at lock time may all be narrower than the
    // widest value the column will show.
    let digit_w = aligned.iter().map(|r| r.2 / 6).max().unwrap_or(12) as i64;
    let left = aligned.iter().map(|r| r.0).min()? as i64 - 8 - 2 * digit_w;
    let right = right_edge + 8;
    // The last act's row sits directly above the timer. If the lowest row
    // that was read is more than a row and a half above the timer crop, the
    // rows below it went unread (highlighted row, a blank comparison) and
    // the block must be anchored lower — otherwise every act reads the row
    // above it and the golds fill with impossible segments.
    let mut last_cy = cy(*col.last()?);
    while timer.1 as i64 - last_cy > pitch * 3 / 2 {
        last_cy += pitch;
    }
    let bottom = last_cy + pitch / 2;
    let top = bottom - pitch * acts.max(1) as i64;
    let splits = (
        (left - timer.0 as i64) as i32,
        (top - timer.1 as i64) as i32,
        (right - left).max(1) as u32,
        (bottom - top) as u32,
    );
    let counter = boxes
        .iter()
        .filter(|(r, w)| {
            let t = w.text.trim();
            t.len() >= 3
                && t.chars().all(|c| c.is_ascii_digit())
                && (r.1 + r.3) as i64 <= top + 4
                && (r.0 + r.2) as i64 > band_x0
                && (r.0 as i64) < band_x1
                && r.3 < timer.3
        })
        .map(|(r, _)| *r)
        .max_by_key(|r| r.1)
        .map(|r| {
            // Right-aligned number: generous headroom on the left so a
            // leading digit never sits on the crop edge (96621 -> 6621).
            (
                r.0 as i64 - 30 - timer.0 as i64,
                r.1 as i64 - 6 - timer.1 as i64,
                r.2 as i64 + 42,
                r.3 as i64 + 12,
            )
        })
        .map(|(dx, dy, w, h)| (dx as i32, dy as i32, w as u32, h as u32));

    // Rows below the timer: label each time with the letter words on its
    // line to the left; the one that reads "Sum of Best" is the SoB.
    let labels: Vec<(R, &ocr::Word)> = letters
        .iter()
        .filter(|w| w.conf >= 30.0 && !time_shaped(&w.text))
        .map(|w| ((w.x / sc, w.y / sc, w.w.max(1) / sc, w.h.max(1) / sc), w))
        .collect();
    // Times below the timer may come from either pass: a shape that parses
    // as a time is trustworthy even at low confidence (a stray glyph merged
    // into the word drags tesseract's score down).
    let below_src: Vec<(R, &ocr::Word)> = words
        .iter()
        .chain(letters.iter())
        .filter(|w| w.conf >= 10.0 && time_shaped(&w.text))
        .map(|w| ((w.x / sc, w.y / sc, w.w.max(1) / sc, w.h.max(1) / sc), w))
        .collect();
    let below: Vec<(R, String)> = below_src
        .iter()
        .filter(|(r, _)| {
            r.1 as i64 >= (timer.1 + timer.3) as i64 - 4
                && (r.0 + r.2) as i64 > band_x0
                && (r.0 as i64) < band_x1
                && r.3 >= 10
                && r.3 < timer.3
        })
        .map(|(r, _)| {
            let label: String = labels
                .iter()
                .filter(|(b, _)| {
                    (cy(*b) - cy(*r)).abs() <= (r.3 as i64).max(6)
                        && (b.0 + b.2) as i64 <= r.0 as i64 + 4
                        && b.0 as i64 + 700 >= r.0 as i64
                })
                .map(|(_, w)| w.text.to_lowercase())
                .collect::<Vec<_>>()
                .join(" ");
            (*r, label)
        })
        .collect();
    if !letters.is_empty() {
        debug!(
            "pane rows below the timer: {:?}",
            below
                .iter()
                .map(|(r, l)| format!("y={} {l:?}", r.1))
                .collect::<Vec<_>>()
        );
    }
    let sob = below
        .iter()
        .find(|(_, label)| {
            (label.contains("best") && (label.contains("sum") || label.contains("segment")))
                || label.contains("sob")
        })
        .map(|(r, _)| {
            (
                (r.0 as i64 - 14 - timer.0 as i64) as i32,
                (r.1 as i64 - 8 - timer.1 as i64) as i32,
                r.2 + 28,
                r.3 + 16,
            )
        });
    Some(PaneGeometry {
        splits,
        counter,
        sob,
        rows_read: col.len(),
        pitch: pitch as u32,
    })
}

/// Measure the pane geometry from the current union crop (see
/// `pane_geometry`): one sparse-text OCR pass at 2x.
async fn measure_pane(
    ocr_engine: &mut OcrEngine,
    union_img: &GrayImage,
    timer: R,
    acts: u32,
    pre: &PreprocessCfg,
    want_sob: bool,
) -> Result<Option<PaneGeometry>> {
    const UP: u32 = 2;
    let pre2 = PreprocessCfg {
        upscale: UP,
        threshold: pre.threshold,
        invert: pre.invert,
    };
    let proc = ocr::preprocess(union_img, &pre2);
    let png = ocr::to_png(&proc)?;
    let words = ocr_engine
        .recognize_words(&png, Some("0123456789:."), 11)
        .await?;
    // The letters pass only serves the SoB label; skip it when not wanted.
    let letters = if want_sob {
        ocr_engine
            .recognize_words(&png, None, 11)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // NG_DUMP_PANE=1 saves what this pass saw (debugging a layout).
    if std::env::var_os("NG_DUMP_PANE").is_some() {
        let _ = std::fs::create_dir_all("calibration");
        let _ = proc.save("calibration/pane.png");
        debug!(
            "pane words (2x px): {:?}",
            words
                .iter()
                .map(|w| format!("{}@{},{} {}x{}", w.text, w.x, w.y, w.w, w.h))
                .collect::<Vec<_>>()
        );
        debug!(
            "pane letters (2x px): {:?}",
            letters
                .iter()
                .map(|w| format!("{}@{},{}", w.text, w.x, w.y))
                .collect::<Vec<_>>()
        );
    }
    Ok(pane_geometry(&words, &letters, UP, timer, acts))
}

/// OCR the splits column as `rows` equal-height rows: one parsed time (or
/// None) per act row. One tesseract call for the whole column (sparse-text
/// mode with word boxes); each word lands in the row its centre falls in,
/// and the rightmost time-shaped word of a row is that row's value. Falls
/// back to one call per row if the column pass produced no words at all.
async fn read_splits_rows(
    ocr_engine: &mut OcrEngine,
    union_img: &GrayImage,
    (sx, sy, sw, sh): R,
    rows: u32,
    pre: &PreprocessCfg,
) -> Result<Vec<Option<i64>>> {
    let panel = image::imageops::crop_imm(union_img, sx, sy, sw, sh).to_image();
    let rows = rows.max(1);
    let row_h = (sh / rows).max(1);
    let proc = ocr::preprocess(&panel, pre);
    let up = pre.upscale.max(1);
    let words = match ocr_engine
        .recognize_words(&ocr::to_png(&proc)?, Some("0123456789:."), 6)
        .await
    {
        Ok(w) => w,
        Err(e) => {
            warn!("splits ocr failed: {e:#}");
            Vec::new()
        }
    };
    // Sparse mode likes to split "2:41.0" at the colon into "2:" and
    // "41.0" — and "41.0" parses as a time on its own. The crop is the
    // cumulative column alone, so a row's words joined in x order are its
    // one value.
    let mut per_row: Vec<Vec<&ocr::Word>> = vec![Vec::new(); rows as usize];
    for w in &words {
        let cy = (w.y + w.h / 2) / up;
        let row = (cy / row_h) as usize;
        if row < rows as usize {
            per_row[row].push(w);
        }
    }
    let mut values: Vec<Option<i64>> = vec![None; rows as usize];
    for (row, ws) in per_row.iter_mut().enumerate() {
        ws.sort_by_key(|w| w.x);
        let joined: String = ws
            .iter()
            .map(|w| w.text.trim())
            .collect::<Vec<_>>()
            .concat();
        values[row] = parse_time(joined.trim_end_matches('.'));
    }
    if words.is_empty() {
        // Sparse mode occasionally returns nothing for a column it would
        // read row by row; pay the six calls rather than lose the read.
        for (i, slot) in values.iter_mut().enumerate() {
            let row = image::imageops::crop_imm(&panel, 0, i as u32 * row_h, sw, row_h).to_image();
            let rp = ocr::preprocess(&row, pre);
            if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&rp)?).await {
                *slot = parse_time(txt.trim());
            }
        }
    }
    Ok(values)
}

/// One position where a timer might be: a layout plus a pixel offset.
struct Candidate {
    layout: usize,
    off: (i32, i32),
    regs: Regions,
    streak: u32,
    /// Last parsed value and the frame time it was seen at.
    last: Option<(i64, i64)>,
}

impl Candidate {
    /// A real timer's readings are consistent between looks: frozen, or
    /// advancing by roughly the elapsed time. Garbage jumps around.
    fn observe(&mut self, v: Option<i64>, t: i64) -> bool {
        let consistent = match (v, self.last) {
            (Some(v), Some((p, pt))) => {
                let elapsed = (t - pt).max(0);
                (v - p).abs() <= 5000 || (v - p - elapsed).abs() <= 5000
            }
            (Some(_), None) => true,
            _ => false,
        };
        self.last = v.map(|v| (v, t));
        if consistent {
            self.streak += 1;
        } else {
            self.streak = 0;
        }
        consistent
    }
}

pub fn capture_cfg(cfg: &Config) -> capture::CaptureCfg {
    let s = &cfg.stream;
    let (ux, uy, uw, uh) = regions(cfg)[0].union;
    capture::CaptureCfg {
        channel: s.channel.clone(),
        quality: s.quality.clone(),
        source: s.source,
        streamlink_extra_args: s.streamlink_extra_args.clone(),
        filter: format!(
            "fps={},scale={}:{}:flags=bicubic,crop={uw}:{uh}:{ux}:{uy}",
            s.fps, s.canvas_w, s.canvas_h
        ),
        // Colour is needed: the timer is converted with the brightest channel
        // (red timers stay bright), the splits column with luma.
        pix_fmt: "rgb24".into(),
        title_filter: s.title_filter.clone(),
        vod_id: s.vod_id.clone(),
        input: s.input.clone(),
        start_secs: s.start_secs,
        frame_len: (uw * uh * 3) as usize,
        frame_timeout_secs: s.frame_timeout_secs,
        offline_poll_secs: s.offline_poll_secs,
        restart_delay_secs: s.restart_delay_secs,
        active_window: s.active_window().ok().flatten(),
        quiet_poll_secs: s.quiet_poll_secs,
    }
}

/// Load the tracked game/category: a `!setgame` persisted in the DB wins over
/// the config file.
pub async fn load_game(pool: &sqlx::SqlitePool, cfg: &Config) -> Result<(String, String)> {
    let game = db::get_setting(pool, "game")
        .await?
        .unwrap_or_else(|| cfg.game.name.clone());
    let category = db::get_setting(pool, "category")
        .await?
        .unwrap_or_else(|| cfg.game.category.clone());
    Ok((game, category))
}

pub async fn run(cfg: Config) -> Result<()> {
    let pool = db::open(&cfg.database.path)
        .await
        .with_context(|| format!("opening database {}", cfg.database.path))?;
    let (game, category) = load_game(&pool, &cfg).await?;
    info!(
        "tracking {game} [{category}] on twitch.tv/{}",
        cfg.stream.channel
    );

    let shared = Arc::new(Shared {
        game: RwLock::new((game, category)),
        status: RwLock::new(Status {
            phase: "IDLE".into(),
            ..Default::default()
        }),
        acts: cfg.game.act_list(),
        current_splits: RwLock::new(Vec::new()),
        record_label: cfg.game.record_label.clone(),
        baseline_best_ms: cfg.game.baseline_best_ms(),
    });

    let mut ocr_engine = OcrEngine::from_config(&cfg.ocr)?;
    let pre = PreprocessCfg::from(&cfg.timer);
    let mut tracker = Tracker::new(cfg.detection.clone());

    let (frame_tx, mut frame_rx) = mpsc::channel::<capture::CaptureEvent>(4);
    let cap = capture_cfg(&cfg);
    tokio::spawn(async move {
        if let Err(e) = capture::capture_loop(cap, frame_tx).await {
            error!("capture loop died: {e:#}");
        }
    });

    let (announce_tx, announce_rx) = mpsc::unbounded_channel::<String>();
    if cfg.chat.enabled {
        let chat_channel = if cfg.chat.channel.trim().is_empty() {
            cfg.stream.channel.clone()
        } else {
            cfg.chat
                .channel
                .trim()
                .trim_start_matches('#')
                .to_ascii_lowercase()
        };
        let ctx = chat::ChatCtx {
            cfg: cfg.chat.clone(),
            channel: chat_channel,
            pool: pool.clone(),
            shared: shared.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = chat::run_chat(ctx, announce_rx).await {
                error!("chat task died: {e:#}");
            }
        });
    }
    let announce = cfg.chat.enabled && cfg.chat.announce;

    // Layouts: probe every layout's timer until one parses consistently, lock
    // to it, and re-probe if the locked layout's timer goes dark for a while
    // (scene switch mid-stream). Lock immediately if there's only one.
    let regs = regions(&cfg);
    let layout_names = layout_names(&cfg);
    let (uw, uh) = (regs[0].union.2, regs[0].union.3);
    // Probe candidates: each layout at its configured position, then at every
    // drift offset. Configured positions are read on every probe frame; the
    // offsets take turns (one per layout per frame), and any offset that has
    // parsed once is read every frame until it locks or fails.
    let offsets = drift_offsets(&cfg.layout_search);
    let mut cands: Vec<Candidate> = Vec::new();
    for (i, r) in regs.iter().enumerate() {
        cands.push(Candidate {
            layout: i,
            off: (0, 0),
            regs: r.clone(),
            streak: 0,
            last: None,
        });
        for &off in &offsets {
            if let Some(sr) = shifted(r, off) {
                cands.push(Candidate {
                    layout: i,
                    off,
                    regs: sr,
                    streak: 0,
                    last: None,
                });
            }
        }
    }
    let mut active_layout: usize = 0;
    let mut active_regs: Regions = regs[0].clone();
    let mut active_off: (i32, i32) = (0, 0);
    let mut layout_locked = cands.len() == 1;
    // Until the first lock no layout is favoured; afterwards the active one is.
    let mut ever_locked = layout_locked;
    let mut probe_rr: usize = 0;
    let mut dark_frames: u32 = 0;
    let dark_frames_search = cfg.layout_search.dark_frames_search.max(1);
    // Fine drift while locked: where the digits SHOULD sit inside each
    // layout's timer crop — right edge at ~94% of the width, vertically
    // centred, which is how every calibrated crop places them. Learning the
    // anchor from the first lock instead would enshrine a bad lock: digits
    // clipped at the crop edge would be "corrected" back to the edge, the
    // timer would keep going dark, and the layout probe would thrash.
    let anchors: Vec<Option<(i32, i32)>> = regs
        .iter()
        .map(|r| {
            Some((
                (r.timer.2 as f32 * 0.94).round() as i32,
                (r.timer.3 as f32 * 0.47).round() as i32,
            ))
        })
        .collect();
    let mut drift_hits: Vec<(i32, i32)> = Vec::new();
    let mut drift_warned = false;
    // Previous frame (brightest-channel union) and what the timer read on it:
    // an unchanged timer crop reuses the reading instead of paying for OCR,
    // and a fully static union skips the probe entirely.
    let mut prev_bright: Option<GrayImage> = None;
    let mut last_text = String::new();
    let mut ocr_skipped: u64 = 0;
    // The splits column as of its last read, and what it said: while the
    // column hasn't changed the previous values are fed again (the tracker
    // still gets its confirmations) without another six OCR calls.
    let mut last_splits_crop: Option<GrayImage> = None;
    let mut last_splits_values: Vec<Option<i64>> = Vec::new();
    // Splits/counter rectangles measured from the frame at the last lock.
    let mut pane_geom: Option<PaneGeometry> = None;
    // Capture health for the open session, flushed to its row every minute.
    let mut health = db::SessionHealth::default();
    let mut last_health_flush_t: i64 = i64::MIN / 2;
    let fine_drift = cfg.layout_search.drift_px > 0;
    let epoch = Instant::now();
    let mut current: Option<CurrentRun> = None;
    let mut splits_tracker: Option<crate::splits::SplitsTracker> = None;
    let mut last_splits_read_t: i64 = i64::MIN / 2;
    let splits_every_ms = (cfg.splits.read_every_secs * 1000) as i64;
    let pre_splits = ocr::PreprocessCfg {
        upscale: cfg.timer.upscale,
        threshold: cfg.splits.threshold,
        invert: cfg.splits.invert,
    };
    let pre_counter = ocr::PreprocessCfg {
        upscale: cfg.timer.upscale,
        threshold: cfg.attempts_counter.threshold,
        invert: cfg.attempts_counter.invert,
    };
    // (candidate counter value, consecutive sightings) — reset at every run
    // start so a stale display can't inherit the previous run's streak.
    let mut counter_stable: Option<(i64, u32)> = None;
    let mut last_counter_read_t: i64 = i64::MIN / 2;
    // The lifetime counter only ever increases; a read at or below the last
    // recorded value is the previous attempt's number still on screen.
    let mut last_ls_attempt: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(ls_attempt) FROM runs")
            .fetch_one(&pool)
            .await?;
    let mut sob_stable: Option<(i64, u32)> = None;
    let mut sob_recorded: Option<i64> = db::get_setting(&pool, "ls_sob_ms")
        .await?
        .and_then(|s| s.parse().ok());
    let mut last_sob_read_t: i64 = i64::MIN / 2;

    // Recorded sources (vod/file) may decode much faster than realtime, so
    // the state machine is ticked by frame index instead of wall clock —
    // detection becomes deterministic and independent of processing speed.
    let recorded = cfg.stream.source.is_recorded();
    let frame_interval_ms = 1000 / cfg.stream.fps as i64;
    let mut frame_idx: i64 = 0;

    // Base timestamp for logging recorded runs on the original broadcast
    // timeline: config wins, else Twitch's createdAt for VODs, else the
    // analysis clock.
    let time_base: Option<i64> = if recorded {
        match cfg.stream.recorded_start_ms()? {
            Some(ms) => Some(ms),
            None if cfg.stream.source == crate::config::SourceKind::Vod => {
                let http = reqwest::Client::new();
                match crate::twitch_hls::vod_created_at(&http, &cfg.stream.vod_id).await {
                    Ok(Some(ms)) => {
                        info!("vod broadcast started {}", util::datetime_of_ms(ms));
                        Some(ms)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        warn!("could not fetch vod start time ({e:#}); using analysis clock");
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };
    // Frame 0 is `start_secs` into the recording when seeking.
    let time_base = time_base.map(|b| b + (cfg.stream.start_secs * 1000.0) as i64);

    let mut obs_log = match &cfg.debug.obs_log {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening debug.obs_log {path}"))?,
        )),
        None => None,
    };

    // One sessions row per broadcast: opened on the first frame, closed when
    // the channel goes offline (or on shutdown/end of input).
    let mut session_id: Option<i64> = None;
    let session_label = match cfg.stream.source {
        crate::config::SourceKind::Vod => format!("vod {}", cfg.stream.vod_id),
        crate::config::SourceKind::File => cfg.stream.input.clone(),
        _ => cfg.stream.channel.clone(),
    };
    let session_source = match cfg.stream.source {
        crate::config::SourceKind::Hls => "hls",
        crate::config::SourceKind::Streamlink => "streamlink",
        crate::config::SourceKind::Vod => "vod",
        crate::config::SourceKind::File => "file",
    };
    let mut last_t: i64 = 0;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let event = tokio::select! {
            _ = &mut ctrl_c => {
                info!("shutting down");
                break;
            }
            maybe = frame_rx.recv() => match maybe {
                Some(ev) => ev,
                None => break,
            },
        };
        let raw = match event {
            capture::CaptureEvent::Frame(raw) => raw,
            capture::CaptureEvent::StreamOffline => {
                let wall_now = time_base.map(|b| b + last_t).unwrap_or_else(util::unix_ms);
                // Broadcast is over: a run still in progress is a DNF.
                if current.is_some() {
                    let last_ms = tracker.smoothed_now(last_t).unwrap_or(0);
                    let ev = Event::Reset {
                        last_ms,
                        reason: crate::state::ResetReason::Disappeared,
                    };
                    if let Err(e) = handle_event(
                        &pool,
                        &shared,
                        &announce_tx,
                        announce,
                        &mut current,
                        ev,
                        wall_now,
                    )
                    .await
                    {
                        warn!("failed to record end-of-stream reset: {e:#}");
                    }
                    tracker = Tracker::new(cfg.detection.clone());
                }
                if let Some(id) = session_id.take() {
                    if let Err(e) = db::update_session_health(&pool, id, &health).await {
                        warn!("failed to update session health: {e:#}");
                    }
                    if let Err(e) = db::close_session(&pool, id, wall_now).await {
                        warn!("failed to close session {id}: {e:#}");
                    } else {
                        info!(
                            "session #{id} closed ({} of {} frames read, {} layout events, {} OCR passes skipped as static)",
                            health.parsed,
                            health.frames,
                            health.events.len(),
                            ocr_skipped
                        );
                    }
                }
                continue;
            }
        };

        let Some(union_rgb) = RgbImage::from_raw(uw, uh, raw) else {
            warn!("dropped a frame with unexpected size");
            continue;
        };
        // Two grayscales of the same crop: luma for the splits column and
        // pane analysis, brightest channel for the timer/counter/SoB.
        let union_img = to_luma(&union_rgb);
        let union_bright = to_bright(&union_rgb);
        let t = if recorded {
            frame_idx * frame_interval_ms
        } else {
            epoch.elapsed().as_millis() as i64
        };
        frame_idx += 1;
        // OCR the timer. When locked, only the active position is read; while
        // probing, candidate positions are read too and the first to parse
        // consistently on five looks becomes the active position.
        let read_timer = |img: &GrayImage, r: (u32, u32, u32, u32)| {
            let g = image::imageops::crop_imm(img, r.0, r.1, r.2, r.3).to_image();
            ocr::to_png(&ocr::preprocess(&g, &pre))
        };
        let mut text = String::new();
        let mut ink: Option<(i32, i32)> = None;
        let mut frame_static = false;
        if layout_locked {
            let r = active_regs.timer;
            if prev_bright
                .as_ref()
                .is_some_and(|p| !region_changed(p, &union_bright, r))
            {
                // Same pixels as last frame: same reading, no OCR.
                text = last_text.clone();
                frame_static = true;
                ocr_skipped += 1;
            } else {
                let g = image::imageops::crop_imm(&union_bright, r.0, r.1, r.2, r.3).to_image();
                let proc = ocr::preprocess(&g, &pre);
                match ocr_engine.recognize_boxed(&ocr::to_png(&proc)?).await {
                    Ok((t, bbox)) => {
                        text = t.trim().to_string();
                        // A trailing "1" is a narrow glyph: its ink ends ~9px
                        // short of the other digits' right edge, and at 2 fps the
                        // hundredths digit repeats for many frames, which would
                        // read as a consistent shift. Measure on other digits.
                        if fine_drift && parse_time(&text).is_some() && !text.ends_with('1') {
                            ink = ink_anchor(&proc, bbox.map(|b| b.0), pre.upscale);
                        }
                    }
                    Err(e) => warn!("ocr failed: {e:#}"),
                }
                last_text = text.clone();
            }
        } else if prev_bright
            .as_ref()
            .is_some_and(|p| !region_changed(p, &union_bright, (0, 0, uw, uh)))
        {
            // Nothing on screen changed since the last probe: whatever the
            // candidates would read, they read last frame. Skip the probe.
            frame_static = true;
            ocr_skipped += 1;
        } else {
            let mut to_read: Vec<usize> = Vec::new();
            for (ci, c) in cands.iter().enumerate() {
                let base = c.off == (0, 0);
                let active = c.layout == active_layout && c.off == active_off;
                let hot = !base && c.streak > 0;
                if base || active || hot {
                    to_read.push(ci);
                }
            }
            // Offset candidates take turns: one per layout per frame, two for
            // the layout that was active — a nudge of the same scene is far
            // likelier than a scene switch, and layouts whose timers overlap
            // would otherwise race to explain the same digits.
            let n_off = offsets.len().max(1);
            let turn = probe_rr % n_off;
            probe_rr = probe_rr.wrapping_add(1);
            for li in 0..regs.len() {
                let favoured = ever_locked && li == active_layout;
                let turns = if favoured {
                    [Some(turn), Some((turn + n_off / 2) % n_off)]
                } else {
                    [Some(turn), None]
                };
                for tn in turns.into_iter().flatten() {
                    let pick = cands
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.layout == li && c.off != (0, 0))
                        .nth(tn)
                        .map(|(ci, _)| ci);
                    if let Some(ci) = pick {
                        if !to_read.contains(&ci) {
                            to_read.push(ci);
                        }
                    }
                }
            }
            let mut winner: Option<(usize, String)> = None;
            for ci in to_read {
                let c = &mut cands[ci];
                let rd = match ocr_engine
                    .recognize(&read_timer(&union_bright, c.regs.timer)?)
                    .await
                {
                    Ok(t) => t.trim().to_string(),
                    Err(_) => String::new(),
                };
                let v = parse_time(&rd);
                // Switching scenes needs a longer streak than re-finding the
                // same scene, so an overlapping rectangle of another layout
                // can't win merely by being tried first.
                let need = if !ever_locked || c.layout == active_layout {
                    5
                } else {
                    10
                };
                if c.observe(v, t) && c.streak >= need && winner.is_none() {
                    winner = Some((ci, rd.clone()));
                }
                if c.layout == active_layout && c.off == active_off {
                    text = rd;
                }
            }
            if let Some((ci, winner_text)) = winner {
                text = winner_text;
                let (mut new_layout, mut new_off, mut new_regs) =
                    (cands[ci].layout, cands[ci].off, cands[ci].regs.clone());
                // Layouts whose timer rectangles overlap can both explain the
                // same digits, but their splits columns sit differently: put
                // every layout's timer where the winner's is and keep the one
                // whose column reads most like times (ties keep the winner).
                if regs.len() > 1 && new_regs.splits.is_some() {
                    let rows = shared.acts.len().max(1) as u32;
                    let win_t = new_regs.timer;
                    let mut best: Option<usize> = None;
                    let mut choice = (new_layout, new_off, new_regs.clone());
                    for (li, r) in regs.iter().enumerate() {
                        let off = (
                            (win_t.0 as i64 - r.timer.0 as i64) as i32,
                            (win_t.1 as i64 - r.timer.1 as i64) as i32,
                        );
                        let Some(sr) = shifted(r, off) else { continue };
                        let Some(splits_rect) = sr.splits else {
                            continue;
                        };
                        let n = read_splits_rows(
                            &mut ocr_engine,
                            &union_img,
                            splits_rect,
                            rows,
                            &pre_splits,
                        )
                        .await?
                        .iter()
                        .filter(|v| v.is_some())
                        .count();
                        debug!(
                            "layout {:?} at {:+},{:+}: {n}/{rows} split rows read",
                            layout_names[li], off.0, off.1
                        );
                        let better = match best {
                            None => true,
                            Some(b) => n > b || (n == b && li == new_layout),
                        };
                        if better {
                            best = Some(n);
                            choice = (li, off, sr);
                        }
                    }
                    if choice.0 != new_layout {
                        info!(
                            "layout {:?} explains the timer too, but its splits column reads ({}/{rows} rows); taking it over {:?}",
                            layout_names[choice.0], best.unwrap_or(0), layout_names[new_layout]
                        );
                    }
                    (new_layout, new_off, new_regs) = choice;
                }
                // Measure where the split rows and counter really are; the
                // configured rectangles are only the fallback.
                if new_regs.splits.is_some() {
                    let acts = shared.acts.len().max(1) as u32;
                    let want_sob = new_regs.sob.is_some();
                    match measure_pane(
                        &mut ocr_engine,
                        &union_img,
                        new_regs.timer,
                        acts,
                        &pre_splits,
                        want_sob,
                    )
                    .await
                    {
                        Ok(Some(g)) => {
                            g.apply(&mut new_regs);
                            let (ux, uy) = (new_regs.union.0, new_regs.union.1);
                            let s = new_regs.splits.unwrap();
                            let at = |r: Option<R>, measured: bool, what: &str| match r {
                                Some(c) if measured => {
                                    format!(", {what} at {},{} {}x{}", ux + c.0, uy + c.1, c.2, c.3)
                                }
                                _ => String::new(),
                            };
                            info!(
                                "pane geometry: {}/{acts} split rows read, pitch {}px; splits at {},{} {}x{} (canvas){}{}",
                                g.rows_read, g.pitch, ux + s.0, uy + s.1, s.2, s.3,
                                at(new_regs.counter, g.counter.is_some(), "counter"),
                                at(new_regs.sob, g.sob.is_some(), "SoB"),
                            );
                            pane_geom = Some(g);
                            health.event(
                                time_base.map(|b| b + t).unwrap_or_else(util::unix_ms),
                                "geometry",
                                format!("{}/{acts} rows, pitch {}px", g.rows_read, g.pitch),
                            );
                        }
                        Ok(None) => {
                            debug!(
                                "pane geometry not measurable here; using configured rectangles"
                            );
                            pane_geom = None;
                        }
                        Err(e) => warn!("pane geometry pass failed: {e:#}"),
                    }
                }
                let name = &layout_names[new_layout];
                let at = time_base.map(|b| b + t).unwrap_or_else(util::unix_ms);
                let geom_note = match pane_geom {
                    Some(g) => format!("; pitch {}px, {} rows", g.pitch, g.rows_read),
                    None => String::new(),
                };
                health.relocks += 1;
                if new_layout != active_layout {
                    info!(
                        "layout switched to {name:?} (offset {:+},{:+})",
                        new_off.0, new_off.1
                    );
                    health.event(
                        at,
                        "switch",
                        format!("{name} {:+},{:+}{geom_note}", new_off.0, new_off.1),
                    );
                    // A layout change invalidates any splits baseline.
                    splits_tracker = None;
                } else if new_off != active_off {
                    info!(
                        "layout {name:?} re-anchored: LiveSplit moved {:+},{:+} px (was {:+},{:+})",
                        new_off.0, new_off.1, active_off.0, active_off.1
                    );
                    health.event(
                        at,
                        "relock",
                        format!("{name} {:+},{:+}{geom_note}", new_off.0, new_off.1),
                    );
                } else {
                    info!(
                        "layout locked: {name:?} (offset {:+},{:+})",
                        new_off.0, new_off.1
                    );
                    health.event(
                        at,
                        "lock",
                        format!("{name} {:+},{:+}{geom_note}", new_off.0, new_off.1),
                    );
                }
                active_layout = new_layout;
                active_off = new_off;
                active_regs = new_regs;
                layout_locked = true;
                ever_locked = true;
                drift_hits.clear();
                drift_warned = false;
                for c in cands.iter_mut() {
                    c.streak = 0;
                    c.last = None;
                }
            }
        }
        prev_bright = Some(union_bright.clone());
        let parsed = parse_time(&text);
        // Resume probing after a dark stretch on the active position: either
        // the scene changed or the LiveSplit window was nudged.
        if cands.len() > 1 {
            if parsed.is_some() {
                dark_frames = 0;
            } else {
                dark_frames += 1;
                if layout_locked && dark_frames >= dark_frames_search {
                    layout_locked = false;
                    dark_frames = 0;
                    info!(
                        "timer dark for {dark_frames_search} frames; probing layouts and offsets"
                    );
                }
            }
        }
        // Fine drift: a nudge too small to break the timer still misaligns the
        // splits/counter crops, so re-anchor on a consistent shift of the ink.
        if let (true, Some(m)) = (layout_locked, ink) {
            match anchors[active_layout] {
                None => {}
                Some(a) => {
                    let d = (m.0 - a.0, m.1 - a.1);
                    let near_last = drift_hits
                        .last()
                        .is_none_or(|l| (l.0 - d.0).abs() <= 2 && (l.1 - d.1).abs() <= 2);
                    if d.0.abs() < 4 && d.1.abs() < 4 {
                        drift_hits.clear();
                    } else if near_last {
                        drift_hits.push(d);
                    } else {
                        drift_hits = vec![d];
                    }
                    if drift_hits.len() >= 8 {
                        let mut xs: Vec<i32> = drift_hits.iter().map(|h| h.0).collect();
                        let mut ys: Vec<i32> = drift_hits.iter().map(|h| h.1).collect();
                        xs.sort_unstable();
                        ys.sort_unstable();
                        let d = (xs[xs.len() / 2], ys[ys.len() / 2]);
                        let new_off = (active_off.0 + d.0, active_off.1 + d.1);
                        match shifted(&regs[active_layout], new_off) {
                            Some(mut nr) => {
                                info!(
                                    "layout {:?} re-anchored: LiveSplit moved {:+},{:+} px (now {:+},{:+} from configured)",
                                    layout_names[active_layout], d.0, d.1, new_off.0, new_off.1
                                );
                                if let Some(g) = pane_geom {
                                    g.apply(&mut nr);
                                }
                                health.relocks += 1;
                                health.event(
                                    time_base.map(|b| b + t).unwrap_or_else(util::unix_ms),
                                    "drift",
                                    format!(
                                        "{:+},{:+} px (now {:+},{:+})",
                                        d.0, d.1, new_off.0, new_off.1
                                    ),
                                );
                                active_off = new_off;
                                active_regs = nr;
                            }
                            None => {
                                if !drift_warned {
                                    warn!(
                                        "LiveSplit moved {:+},{:+} px, beyond layout_search.drift_px; crops stay put",
                                        d.0, d.1
                                    );
                                    drift_warned = true;
                                }
                            }
                        }
                        drift_hits.clear();
                    }
                }
            }
        }
        let reg = &active_regs;
        let obs = parsed.map(Obs::Time).unwrap_or(Obs::Illegible);
        last_t = t;
        health.frames += 1;
        if parsed.is_some() {
            health.parsed += 1;
        }
        if !layout_locked {
            health.probing += 1;
        }

        if session_id.is_none() {
            let started = time_base.map(|b| b + t).unwrap_or_else(util::unix_ms);
            match db::open_session(
                &pool,
                started,
                session_source,
                &session_label,
                cfg.stream.session_tag.as_deref(),
            )
            .await
            {
                Ok(id) => {
                    info!("session #{id} opened ({session_source}: {session_label})");
                    session_id = Some(id);
                    health = db::SessionHealth::default();
                    last_health_flush_t = t;
                }
                Err(e) => warn!("failed to open session: {e:#}"),
            }
        }
        debug!(
            "frame #{frame_idx} @{t}ms ocr={text:?} parsed={:?} phase={}",
            parsed,
            tracker.phase_name()
        );
        let events = tracker.observe(t, obs);

        // Splits panel pass: only while a run is in progress, on a slow
        // cadence (splits change at most once per act).
        let mut splits_values: Option<Vec<Option<i64>>> = None;
        if let (Some(st), Some(splits_rect)) = (splits_tracker.as_mut(), reg.splits) {
            if t - last_splits_read_t >= splits_every_ms {
                last_splits_read_t = t;
                let rows = shared.acts.len().max(1) as u32;
                let (sx, sy, sw, sh) = splits_rect;
                let crop = image::imageops::crop_imm(&union_img, sx, sy, sw, sh).to_image();
                let unchanged = last_splits_crop
                    .as_ref()
                    .is_some_and(|p| !region_changed(p, &crop, (0, 0, sw, sh)))
                    && last_splits_values.len() == rows as usize;
                let values = if unchanged {
                    // Column unchanged since the last read: same values, no
                    // OCR — the tracker still gets its confirmation.
                    ocr_skipped += 1;
                    last_splits_values.clone()
                } else {
                    let v = read_splits_rows(
                        &mut ocr_engine,
                        &union_img,
                        splits_rect,
                        rows,
                        &pre_splits,
                    )
                    .await?;
                    last_splits_crop = Some(crop);
                    last_splits_values = v.clone();
                    v
                };
                for (idx, cum) in st.observe(&values, tracker.smoothed_now(t)) {
                    let act_name = shared
                        .acts
                        .get(idx)
                        .map(|a| a.0.clone())
                        .unwrap_or_else(|| format!("Act {}", idx + 1));
                    // A split far below the act's configured boundary is a
                    // misread column (wrong row, wrong pane), not a segment
                    // nobody has ever run; keep it out of the golds.
                    let floor = shared
                        .acts
                        .get(idx)
                        .and_then(|a| a.1)
                        .map(|end| end * 6 / 10);
                    if let Some(floor) = floor.filter(|&f| cum < f) {
                        warn!(
                            "ignoring implausible split: {act_name} at {} (below {} — misread column?)",
                            format_ms(cum),
                            format_ms(floor)
                        );
                        continue;
                    }
                    // A split happened in the past: it can't be later than the
                    // timer, and it must come after the previous act's split.
                    let now = tracker.smoothed_now(t);
                    if let Some(now) = now.filter(|&n| cum > n + 5_000) {
                        warn!(
                            "ignoring implausible split: {act_name} at {} while the timer reads {}",
                            format_ms(cum),
                            format_ms(now)
                        );
                        continue;
                    }
                    let prev_cum = current.as_ref().and_then(|cr| {
                        cr.splits
                            .iter()
                            .filter(|s| s.act_index < idx)
                            .map(|s| s.cumulative_ms)
                            .max()
                    });
                    if let Some(p) = prev_cum.filter(|&p| cum <= p) {
                        warn!(
                            "ignoring implausible split: {act_name} at {} is not after the previous act ({})",
                            format_ms(cum),
                            format_ms(p)
                        );
                        continue;
                    }
                    info!("split: {act_name} done at {}", format_ms(cum));
                    let rs = crate::splits::RecordedSplit {
                        act_index: idx,
                        act_name,
                        cumulative_ms: cum,
                    };
                    if let Some(cr) = current.as_mut() {
                        cr.splits.push(rs.clone());
                    }
                    shared.current_splits.write().await.push(rs);
                }
                splits_values = Some(values);
            }
        }

        if let Some(w) = obs_log.as_mut() {
            use std::io::Write;
            let mut line = serde_json::json!({
                "unix_ms": util::unix_ms(),
                "frame": frame_idx,
                "t_ms": t,
                "ocr": text,
                "parsed_ms": parsed,
                "phase": tracker.phase_name(),
                "smoothed_ms": tracker.smoothed_now(t),
                "events": events.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>(),
                "layout": if layout_locked { layout_names[active_layout].as_str() } else { "probing" },
                "offset": [active_off.0, active_off.1],
                "ink": ink.map(|(r, cy)| vec![r, cy]),
                "anchor": anchors[active_layout].map(|(r, cy)| vec![r, cy]),
                "static": frame_static,
            });
            if let Some(v) = &splits_values {
                line["splits"] = serde_json::json!(v);
            }
            // Flush per line so `tail -f` shows frames as they happen.
            if let Err(e) = writeln!(w, "{line}").and_then(|_| w.flush()) {
                warn!("obs_log write failed: {e}");
            }
        }

        // LiveSplit attempt-counter pass: only while a run needs one, on the
        // slow cadence; requires two matching reads before it's trusted.
        if let (Some((cx, cy, cw2, ch2)), Some(cr)) = (reg.counter, current.as_mut()) {
            // Read fast (every 2s) until the run's number is captured — short
            // runs are the ones that used to slip through without one.
            if cr.ls_attempt.is_none() && t - last_counter_read_t >= 2000 {
                last_counter_read_t = t;
                let cimg = image::imageops::crop_imm(&union_bright, cx, cy, cw2, ch2).to_image();
                let cp = ocr::preprocess(&cimg, &pre_counter);
                if std::env::var_os("NG_DUMP_PANE").is_some() {
                    let _ = cp.save("calibration/counter.png");
                }
                if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&cp)?).await {
                    debug!("counter ocr: {:?} (crop {cx},{cy} {cw2}x{ch2})", txt.trim());
                    // The counter only ever grows, by a little per run. A
                    // value that jumps far ahead (or the very first value of
                    // a session, which nothing vouches for) needs three
                    // identical reads instead of two, so one garbage read
                    // can't poison the sequence for the rest of the day.
                    if let Some(v) = crate::timeparse::parse_counter(txt.trim())
                        .filter(|&v| last_ls_attempt.is_none_or(|p| v > p))
                    {
                        let near = last_ls_attempt.is_some_and(|p| v < p + 500);
                        let need = if near { 2 } else { 3 };
                        counter_stable = match counter_stable {
                            Some((pv, n)) if pv == v => Some((v, n + 1)),
                            _ => Some((v, 1)),
                        };
                        if matches!(counter_stable, Some((_, n)) if n >= need) {
                            info!("livesplit attempt counter: {v}");
                            if cr.ls_attempt.is_none() {
                                health.counter_reads += 1;
                            }
                            cr.ls_attempt = Some(v);
                            last_ls_attempt = Some(v);
                        }
                    }
                }
            }
        }

        // Lifetime Sum of Best row: static text that changes only when he
        // golds a segment; a slow read keeps it current in settings.
        if let Some((bx, by, bw2, bh2)) = reg.sob {
            if t - last_sob_read_t >= 60_000 {
                last_sob_read_t = t;
                let bimg = image::imageops::crop_imm(&union_bright, bx, by, bw2, bh2).to_image();
                let bp = ocr::preprocess(&bimg, &pre_counter);
                if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&bp)?).await {
                    // Plausibility: a Sum of Best can never exceed the record
                    // (and can't be wildly below it) — this rejects consistent
                    // misreads when the layout shifts under the crop.
                    let bound = shared.baseline_best_ms.unwrap_or(i64::MAX);
                    if let Some(v) = parse_time(txt.trim()).filter(|&v| v <= bound && v > bound / 2)
                    {
                        sob_stable = match sob_stable {
                            Some((pv, n)) if pv == v => Some((v, n + 1)),
                            _ => Some((v, 1)),
                        };
                        if matches!(sob_stable, Some((_, n)) if n >= 2) && sob_recorded != Some(v) {
                            info!("season sum of best: {}", format_ms(v));
                            if let Err(e) =
                                db::set_setting(&pool, "ls_sob_ms", &v.to_string()).await
                            {
                                warn!("failed to store lifetime SoB: {e:#}");
                            } else {
                                sob_recorded = Some(v);
                            }
                        }
                    }
                }
            }
        }

        {
            let mut st = shared.status.write().await;
            st.phase = tracker.phase_name().to_string();
            st.smoothed_ms = tracker.smoothed_now(t);
            st.read_age_ms = tracker.accepted_age_ms(t);
            st.last_ocr = (!text.is_empty()).then_some(text);
            st.updated_unix_ms = util::unix_ms();
            st.parse_pct =
                (health.frames > 0).then(|| health.parsed as f64 * 100.0 / health.frames as f64);
            st.layout = if layout_locked {
                format!(
                    "{} {:+},{:+}",
                    layout_names[active_layout], active_off.0, active_off.1
                )
            } else {
                "probing".to_string()
            };
        }

        let wall_now = time_base.map(|b| b + t).unwrap_or_else(util::unix_ms);
        if let Some(id) = session_id {
            if t - last_health_flush_t >= 60_000 {
                last_health_flush_t = t;
                if let Err(e) = db::update_session_health(&pool, id, &health).await {
                    warn!("failed to update session health: {e:#}");
                }
            }
        }
        for ev in events {
            // Splits tracker follows the run lifecycle: fresh baseline per
            // run, dropped when the run ends.
            match &ev {
                Event::Started { .. } => {
                    counter_stable = None;
                    if cfg.splits.enabled {
                        splits_tracker = Some(crate::splits::SplitsTracker::new(
                            shared.acts.len(),
                            cfg.splits.tolerance_ms,
                            cfg.splits.confirmations,
                        ));
                        last_splits_read_t = i64::MIN / 2;
                    }
                    shared.current_splits.write().await.clear();
                }
                Event::Finished { .. } | Event::Reset { .. } => {
                    splits_tracker = None;
                    shared.current_splits.write().await.clear();
                }
                // Same run continues on a slipped clock: keep splits state.
                Event::Resynced { .. } => {}
            }
            if let Err(e) = handle_event(
                &pool,
                &shared,
                &announce_tx,
                announce,
                &mut current,
                ev,
                wall_now,
            )
            .await
            {
                // Don't let a transient DB hiccup kill the tracker.
                warn!("failed to record event: {e:#}");
            }
        }
        if let (Some(cr), Some(sid)) = (current.as_mut(), session_id) {
            if cr.session_id.is_none() {
                cr.session_id = Some(sid);
            }
        }
    }
    // Shutdown or end of input: close the session; a run still in progress
    // on a LIVE stream is simply not recorded (it's still happening).
    if let Some(id) = session_id.take() {
        let wall_now = time_base.map(|b| b + last_t).unwrap_or_else(util::unix_ms);
        if let Err(e) = db::update_session_health(&pool, id, &health).await {
            warn!("failed to update session health: {e:#}");
        }
        if let Err(e) = db::close_session(&pool, id, wall_now).await {
            warn!("failed to close session {id}: {e:#}");
        }
    }
    Ok(())
}

async fn handle_event(
    pool: &sqlx::SqlitePool,
    shared: &Arc<Shared>,
    announce_tx: &mpsc::UnboundedSender<String>,
    announce: bool,
    current: &mut Option<CurrentRun>,
    ev: Event,
    now: i64,
) -> Result<()> {
    match ev {
        Event::Started { timer_ms } => {
            let (game, category) = shared.game.read().await.clone();
            let attempt_number = db::next_attempt_number(pool, &game, &category).await?;
            info!(
                "run started: {game} [{category}] attempt #{attempt_number} (timer at {})",
                format_ms(timer_ms)
            );
            db::log_transition(
                pool,
                now,
                "IDLE",
                "RUNNING",
                &game,
                &category,
                &format!("timer_ms={timer_ms}"),
            )
            .await?;
            *current = Some(CurrentRun {
                game,
                category,
                attempt_number,
                // The timer already shows `timer_ms`, so the run actually
                // began that long ago (also correct when joining mid-run).
                started_unix_ms: now - timer_ms,
                session_id: None, // patched in by the frame loop
                ls_attempt: None,
                splits: Vec::new(),
            });
        }
        Event::Finished { final_ms } => {
            let Some(mut run) = current.take() else {
                warn!("finish event with no run in progress; ignoring");
                return Ok(());
            };
            // The final act's split IS the finish; the run usually ends
            // before the slow splits cadence can confirm the row change.
            let n_acts = shared.acts.len();
            if n_acts > 0
                && !run.splits.is_empty()
                && !run.splits.iter().any(|s| s.act_index == n_acts - 1)
            {
                run.splits.push(crate::splits::RecordedSplit {
                    act_index: n_acts - 1,
                    act_name: shared.acts[n_acts - 1].0.clone(),
                    cumulative_ms: final_ms,
                });
            }
            let tracked_best = db::personal_best(pool, &run.game, &run.category)
                .await?
                .and_then(|r| r.final_time_ms);
            // The record to beat includes any pre-tracking baseline, so we
            // never announce a "record" the runner has already beaten.
            let prior_pb = match (tracked_best, shared.baseline_best_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let is_pb = prior_pb.map(|b| final_ms < b).unwrap_or(true);
            let run_id = db::insert_run(
                pool,
                NewRun {
                    game: &run.game,
                    category: &run.category,
                    attempt_number: run.attempt_number,
                    started_at_ms: run.started_unix_ms,
                    ended_at_ms: now,
                    outcome: db::OUTCOME_FINISHED,
                    reset_reason: None,
                    final_time_ms: Some(final_ms),
                    last_timer_ms: Some(final_ms),
                    session_id: run.session_id,
                    ls_attempt: run.ls_attempt,
                },
            )
            .await?;
            if !run.splits.is_empty() {
                db::insert_splits(pool, run_id, &run.splits).await?;
            }
            db::log_transition(
                pool,
                now,
                "RUNNING",
                "FINISHED",
                &run.game,
                &run.category,
                &format!("final_ms={final_ms}"),
            )
            .await?;
            let label = &shared.record_label;
            // His own LiveSplit counter is the run's identity when we have it.
            let run_no = match run.ls_attempt {
                Some(n) => format!("run {n}"),
                None => format!("tracked #{}", run.attempt_number),
            };
            let msg = if is_pb {
                format!(
                    "Run finished in {} — NEW {label} for {} [{}]! ({run_no})",
                    format_ms(final_ms),
                    run.game,
                    run.category,
                )
            } else {
                format!(
                    "Run finished in {} ({} [{}], {run_no}; {label} is {})",
                    format_ms(final_ms),
                    run.game,
                    run.category,
                    format_ms(prior_pb.unwrap_or(0))
                )
            };
            info!("{msg}");
            if announce {
                let _ = announce_tx.send(msg);
            }
        }
        Event::Resynced { from_ms, to_ms } => {
            let (game, category) = shared.game.read().await.clone();
            info!(
                "stream clock slipped: re-anchored {} -> {} (same run continues)",
                format_ms(from_ms),
                format_ms(to_ms)
            );
            db::log_transition(
                pool,
                now,
                "RUNNING",
                "RUNNING",
                &game,
                &category,
                &format!("resync from_ms={from_ms} to_ms={to_ms}"),
            )
            .await?;
        }
        Event::Reset { last_ms, reason } => {
            let Some(run) = current.take() else {
                warn!("reset event with no run in progress; ignoring");
                return Ok(());
            };
            let run_id = db::insert_run(
                pool,
                NewRun {
                    game: &run.game,
                    category: &run.category,
                    attempt_number: run.attempt_number,
                    started_at_ms: run.started_unix_ms,
                    ended_at_ms: now,
                    outcome: db::OUTCOME_RESET,
                    reset_reason: Some(reason.as_str()),
                    final_time_ms: None,
                    last_timer_ms: Some(last_ms),
                    session_id: run.session_id,
                    ls_attempt: run.ls_attempt,
                },
            )
            .await?;
            // Splits of dead runs still feed gold-segment stats.
            if !run.splits.is_empty() {
                db::insert_splits(pool, run_id, &run.splits).await?;
            }
            db::log_transition(
                pool,
                now,
                "RUNNING",
                "RESET",
                &run.game,
                &run.category,
                &format!("last_ms={last_ms} reason={}", reason.as_str()),
            )
            .await?;
            info!(
                "run reset at {} ({}) — attempt #{}",
                format_ms(last_ms),
                reason.as_str(),
                run.attempt_number
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LayoutSearchCfg;

    fn regs() -> Regions {
        Regions {
            union: (0, 0, 500, 300),
            timer: (40, 40, 100, 50),
            splits: Some((200, 20, 60, 200)),
            counter: None,
            sob: Some((10, 250, 80, 30)),
        }
    }

    #[test]
    fn drift_offsets_nearest_first_without_origin() {
        let offs = drift_offsets(&LayoutSearchCfg {
            drift_px: 24,
            step_px: 12,
            dark_frames_search: 30,
        });
        assert_eq!(offs.len(), 24);
        assert!(!offs.contains(&(0, 0)));
        // First ring (Chebyshev distance 12) precedes the second (24); axis
        // neighbours come before diagonals within a ring.
        assert_eq!(offs[0].0.abs().max(offs[0].1.abs()), 12);
        assert!(offs[..8].iter().all(|&(x, y)| x.abs().max(y.abs()) == 12));
        assert!(offs[8..].iter().all(|&(x, y)| x.abs().max(y.abs()) == 24));
        assert!(offs[..4].iter().all(|&(x, y)| x == 0 || y == 0));
        assert!(drift_offsets(&LayoutSearchCfg {
            drift_px: 0,
            step_px: 12,
            dark_frames_search: 30
        })
        .is_empty());
    }

    #[test]
    fn shifted_moves_every_rect_or_nothing() {
        let r = regs();
        let s = shifted(&r, (12, -12)).unwrap();
        assert_eq!(s.timer, (52, 28, 100, 50));
        assert_eq!(s.splits, Some((212, 8, 60, 200)));
        assert_eq!(s.sob, Some((22, 238, 80, 30)));
        assert_eq!(s.counter, None);
        // The SoB row sits 20px above the bottom edge: +24 down leaves the union.
        assert!(shifted(&r, (0, 24)).is_none());
        // The SoB row starts at x=10: -12 left leaves the union.
        assert!(shifted(&r, (-12, 0)).is_none());
    }

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

    #[test]
    fn pane_geometry_from_split_rows() {
        // Union-relative, scale 1. Timer at (60, 400, 300, 90); six rows of
        // 45px above it with delta + cumulative columns, counter on top.
        let timer = (60, 400, 300, 90);
        let mut words = vec![word(330, 60, 60, 20, "96454")];
        for i in 0..6u32 {
            let y = 100 + i * 45;
            words.push(word(180, y, 50, 20, "+0.2"));
            words.push(word(260, y, 70, 20, &format!("{}:47.6", i + 1)));
        }
        // Noise: the timer itself, an integer far to the right, a tiny time.
        words.push(word(120, 410, 220, 70, "1:41.26"));
        words.push(word(900, 60, 60, 20, "12345"));
        words.push(word(270, 380, 20, 6, "0.1"));
        // Below the timer: a "Previous Segment" row and the Sum of Best row.
        words.push(word(300, 520, 60, 20, "0.3"));
        words.push(word(290, 560, 70, 20, "11:31.7"));
        let letters = vec![
            word(60, 520, 90, 20, "Previous"),
            word(160, 520, 90, 20, "Segment"),
            word(60, 560, 50, 20, "Sum"),
            word(115, 560, 30, 20, "of"),
            word(150, 560, 50, 20, "Best"),
            word(205, 560, 80, 20, "Segments"),
        ];
        let g = pane_geometry(&words, &letters, 1, timer, 6).expect("geometry");
        assert_eq!(
            g.sob,
            Some((290 - 14 - 60, 560 - 8 - 400, 70 + 28, 20 + 16))
        );
        assert_eq!(pane_geometry(&words, &[], 1, timer, 6).unwrap().sob, None);
        assert_eq!(g.pitch, 45);
        assert_eq!(g.rows_read, 6);
        // Column spans the cumulative words (260..330) with 8px margins plus
        // two digits' width (2 x 70/6 = 22px) of headroom on the left.
        assert_eq!(g.splits.0, 260 - 8 - 22 - 60);
        assert_eq!(g.splits.2, 338 - (260 - 8 - 22));
        // Block: bottom = last row centre (335) + 22 = 357; top = 357 - 270 = 87.
        assert_eq!(g.splits.3, 270);
        assert_eq!(g.splits.1, 87 - 400);
        let c = g.counter.expect("counter");
        assert_eq!(
            (c.0, c.1, c.2, c.3),
            (330 - 30 - 60, 60 - 6 - 400, 60 + 42, 32)
        );
        // One row is not enough; an absurd pitch is rejected.
        assert!(pane_geometry(&words[..2], &[], 1, timer, 6).is_none());
        let far = vec![
            word(260, 100, 70, 20, "0:47.6"),
            word(260, 300, 70, 20, "2:44.2"),
        ];
        assert!(pane_geometry(&far, &[], 1, timer, 6).is_none());
    }

    #[test]
    fn region_changed_ignores_noise_but_sees_a_digit() {
        let a = GrayImage::from_pixel(400, 100, image::Luma([20]));
        // Compression noise: every pixel wobbles by a few levels.
        let noisy = GrayImage::from_fn(400, 100, |x, y| image::Luma([20 + ((x + y) % 7) as u8]));
        assert!(!region_changed(&a, &noisy, (0, 0, 400, 100)));
        // A hundredths digit flipping: a 20x40 block goes bright.
        let mut digit = a.clone();
        for y in 30..70 {
            for x in 360..380 {
                digit.put_pixel(x, y, image::Luma([255]));
            }
        }
        assert!(region_changed(&a, &digit, (0, 0, 400, 100)));
        // ...but not if the rectangle we care about excludes it.
        assert!(!region_changed(&a, &digit, (0, 0, 300, 100)));
        // Different dimensions can't be compared: treat as changed.
        let small = GrayImage::from_pixel(10, 10, image::Luma([20]));
        assert!(region_changed(&a, &small, (0, 0, 10, 10)));
    }

    #[test]
    fn ink_anchor_ignores_pane_border() {
        // 4x-upscaled 100x25 crop: digits occupy x 40..120 (crop px 10..30),
        // y 20..60 (crop px 5..15); a full-height border at x 140..146.
        let (w, h) = (400u32, 100u32);
        let mut img = GrayImage::from_pixel(w, h, image::Luma([255]));
        for y in 20..60 {
            for x in 40..120 {
                img.put_pixel(x, y, image::Luma([0]));
            }
        }
        for y in 0..h {
            for x in 140..146 {
                img.put_pixel(x, y, image::Luma([0]));
            }
        }
        // Blur next to the border must not count as digits either.
        for y in 0..h {
            img.put_pixel(138, y, image::Luma([0]));
        }
        assert_eq!(ink_anchor(&img, Some(40), 4), Some((30, 10)));
        assert_eq!(ink_anchor(&img, None, 4), Some((30, 10)));
        // No digits at all -> nothing to anchor on.
        let blank = GrayImage::from_pixel(w, h, image::Luma([255]));
        assert_eq!(ink_anchor(&blank, None, 4), None);
    }

    #[test]
    fn candidate_accepts_frozen_or_advancing_readings() {
        let mut c = Candidate {
            layout: 0,
            off: (0, 0),
            regs: regs(),
            streak: 0,
            last: None,
        };
        assert!(c.observe(Some(10_000), 0));
        assert!(c.observe(Some(10_500), 500)); // advancing with the clock
        assert!(c.observe(Some(22_400), 12_500)); // advanced ~12s after a 12s gap
        assert!(c.observe(Some(22_400), 25_000)); // frozen (paused / pre-start)
        assert_eq!(c.streak, 4);
        assert!(!c.observe(Some(500_000), 13_000)); // garbage jump
        assert_eq!(c.streak, 0);
        assert!(!c.observe(None, 13_500));
    }
}
