//! `locate`: find the LiveSplit pane in a frame instead of measuring crops by
//! hand. The whole canvas-scaled frame goes through tesseract in sparse-text
//! mode (word boxes); the time-shaped words are the pane — the biggest one is
//! the timer, the small ones above it are the split rows, a bare integer above
//! those is the attempt counter, and labelled rows below the timer hold the
//! sum of best. Prints the rectangles as a `[[layouts]]` entry, how far the
//! pane sits from every configured layout, and draws the boxes into
//! calibration/locate.png.

use anyhow::{bail, Context, Result};
use image::{GrayImage, Rgb, RgbImage};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::Config;
use crate::ocr::{self, CliOcr, PreprocessCfg, Word};
use crate::timeparse::{parse_counter, parse_time};
use crate::{app, capture};

const OUT_DIR: &str = "calibration";
type R = (u32, u32, u32, u32);

pub async fn run(cfg: Config, image: Option<PathBuf>, max_frames: u32) -> Result<()> {
    std::fs::create_dir_all(OUT_DIR).context("creating calibration/ dir")?;
    let engine = CliOcr::new(&cfg.ocr).context("locate needs the tesseract CLI")?;
    let (cw, ch) = (cfg.stream.canvas_w, cfg.stream.canvas_h);

    if let Some(path) = image {
        let img = image::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let mut gray = img.to_luma8();
        if gray.dimensions() != (cw, ch) {
            gray = image::imageops::resize(&gray, cw, ch, image::imageops::FilterType::Triangle);
        }
        return match analyze(&gray, &cfg, &engine).await? {
            Some(found) => report(&found, &gray, &cfg),
            None => bail!("no timer-shaped text found in {}", path.display()),
        };
    }

    // Grab whole frames from the configured source, a few seconds apart.
    let mut cap = app::capture_cfg(&cfg);
    cap.filter = format!("fps={},scale={}:{}:flags=bicubic", cfg.stream.fps, cw, ch);
    cap.pix_fmt = "gray".into();
    cap.frame_len = (cw * ch) as usize;
    let (tx, mut rx) = mpsc::channel::<capture::CaptureEvent>(4);
    tokio::spawn(async move {
        if let Err(e) = capture::capture_loop(cap, tx).await {
            error!("capture loop died: {e:#}");
        }
    });
    let every = (cfg.stream.fps as u64 * 5).max(1);
    let mut n: u64 = 0;
    let mut analyzed = 0u32;
    while let Some(ev) = rx.recv().await {
        let raw = match ev {
            capture::CaptureEvent::Frame(raw) => raw,
            capture::CaptureEvent::StreamOffline => {
                println!("{} is offline; waiting for frames", cfg.stream.channel);
                continue;
            }
        };
        n += 1;
        if !(n - 1).is_multiple_of(every) {
            continue;
        }
        let Some(gray) = GrayImage::from_raw(cw, ch, raw) else {
            continue;
        };
        analyzed += 1;
        info!("analyzing frame {n} ({analyzed}/{max_frames})");
        if let Some(found) = analyze(&gray, &cfg, &engine).await? {
            return report(&found, &gray, &cfg);
        }
        gray.save(format!("{OUT_DIR}/locate-last.png"))?;
        if analyzed >= max_frames {
            bail!(
                "no timer-shaped text in {analyzed} frames; last frame saved to \
                 {OUT_DIR}/locate-last.png — is LiveSplit on screen?"
            );
        }
    }
    bail!("input ended before a timer was found")
}

/// Everything the analysis pinned down, in canvas pixels.
struct Found {
    timer_ink: R,
    timer: R,
    split_rows: Vec<R>,
    splits: Option<R>,
    row_pitch: Option<u32>,
    counter: Option<R>,
    below: Vec<(R, String)>,
    sob: Option<R>,
    time_words: Vec<R>,
}

fn bbox(w: &Word, scale: u32) -> R {
    (w.x / scale, w.y / scale, w.w.max(1) / scale, w.h.max(1) / scale)
}

fn is_time(text: &str) -> bool {
    let t = text.trim().trim_end_matches('.').trim_start_matches(['-', '+', '−']);
    t.contains(['.', ':']) && parse_time(t).is_some()
}

fn is_counter(text: &str) -> bool {
    let t = text.trim();
    t.len() >= 3 && t.chars().all(|c| c.is_ascii_digit()) && parse_counter(t).is_some()
}

fn center_y(r: R) -> i64 {
    r.1 as i64 + r.3 as i64 / 2
}

fn h_overlap(a: R, b: R) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2
}

fn grow(r: R, dx: i64, dy: i64, cw: u32, ch: u32) -> R {
    let x = (r.0 as i64 - dx).max(0);
    let y = (r.1 as i64 - dy).max(0);
    let right = (r.0 as i64 + r.2 as i64 + dx).min(cw as i64);
    let bottom = (r.1 as i64 + r.3 as i64 + dy).min(ch as i64);
    (x as u32, y as u32, (right - x).max(1) as u32, (bottom - y).max(1) as u32)
}

fn union_rects(rs: &[R]) -> Option<R> {
    let x = rs.iter().map(|r| r.0).min()?;
    let y = rs.iter().map(|r| r.1).min()?;
    let right = rs.iter().map(|r| r.0 + r.2).max()?;
    let bottom = rs.iter().map(|r| r.1 + r.3).max()?;
    Some((x, y, right - x, bottom - y))
}

async fn analyze(gray: &GrayImage, cfg: &Config, engine: &CliOcr) -> Result<Option<Found>> {
    const UP: u32 = 2;
    let (cw, ch) = gray.dimensions();
    let pre = PreprocessCfg { upscale: UP, threshold: cfg.timer.threshold, invert: cfg.timer.invert };
    let proc = ocr::preprocess(gray, &pre);
    let png = ocr::to_png(&proc)?;
    let digits = engine.recognize_words(&png, Some("0123456789:.-+")).await?;

    let times: Vec<(R, &Word)> = digits
        .iter()
        .filter(|w| is_time(&w.text) && w.conf >= 30.0)
        .map(|w| (bbox(w, UP), w))
        .collect();
    // The timer is the tallest time on screen; anything under 20px is noise.
    let Some((timer_ink, _)) = times.iter().max_by_key(|(r, _)| r.3).copied() else {
        return Ok(None);
    };
    if timer_ink.3 < 20 {
        return Ok(None);
    }
    // Timer crop: room on the left for an extra digit (9:59 -> 10:00) and
    // generous vertical slack.
    let extra = (timer_ink.2 as i64 * 35 / 100).max(40);
    let timer = {
        let x = (timer_ink.0 as i64 - extra).max(0);
        let right = (timer_ink.0 as i64 + timer_ink.2 as i64 + 12).min(cw as i64);
        let y = (timer_ink.1 as i64 - timer_ink.3 as i64 / 3).max(0);
        let bottom = (timer_ink.1 as i64 + timer_ink.3 as i64 * 4 / 3).min(ch as i64);
        (x as u32, y as u32, (right - x) as u32, (bottom - y) as u32)
    };

    // The pane spans a band around the timer's horizontal extent.
    let pane_x = grow(timer_ink, timer_ink.2 as i64 * 3 / 2, 0, cw, ch);
    let small: Vec<R> = times
        .iter()
        .filter(|(r, _)| r.3 < timer_ink.3 * 4 / 5 && h_overlap(*r, pane_x))
        .map(|(r, _)| *r)
        .collect();

    // Split rows: small times above the timer, grouped by line, rightmost
    // word per line = the cumulative-time column.
    let mut above: Vec<R> = small.iter().filter(|r| r.1 + r.3 <= timer_ink.1).copied().collect();
    above.sort_by_key(|r| center_y(*r));
    let mut rows: Vec<Vec<R>> = Vec::new();
    for r in above {
        match rows.last_mut() {
            Some(row) if (center_y(row[0]) - center_y(r)).abs() <= (r.3 as i64 / 2).max(4) => row.push(r),
            _ => rows.push(vec![r]),
        }
    }
    let split_rows: Vec<R> = rows
        .iter()
        .map(|row| *row.iter().max_by_key(|r| r.0 + r.2).unwrap())
        .collect();
    let row_pitch = if split_rows.len() >= 2 {
        let mut gaps: Vec<i64> = split_rows.windows(2).map(|w| center_y(w[1]) - center_y(w[0])).collect();
        gaps.sort_unstable();
        Some(gaps[gaps.len() / 2] as u32)
    } else {
        None
    };
    let splits = union_rects(&split_rows).map(|u| {
        let pad_y = row_pitch.map(|p| p as i64 / 2).unwrap_or(10);
        grow(u, 10, pad_y, cw, ch)
    });

    // Attempt counter: a bare integer above the split rows (or above the
    // timer when no rows were read), inside the pane band.
    let top_limit = split_rows.first().map(|r| r.1).unwrap_or(timer_ink.1);
    let counter = digits
        .iter()
        .filter(|w| is_counter(&w.text) && w.conf >= 30.0)
        .map(|w| bbox(w, UP))
        .filter(|r| r.1 + r.3 <= top_limit && h_overlap(*r, pane_x) && r.3 < timer_ink.3)
        .max_by_key(|r| r.1)
        .map(|r| grow(r, 12, 6, cw, ch));

    // Rows below the timer, labelled with a letters pass so the sum-of-best
    // row can be told from "previous segment" / "PB".
    let below_times: Vec<R> = small.iter().filter(|r| r.1 >= timer_ink.1 + timer_ink.3).copied().collect();
    let mut below: Vec<(R, String)> = Vec::new();
    if !below_times.is_empty() {
        let letters = engine.recognize_words(&png, None).await.unwrap_or_default();
        for r in below_times {
            let mut label: Vec<(u32, String)> = letters
                .iter()
                .filter(|w| w.conf >= 30.0 && !is_time(&w.text))
                .map(|w| (bbox(w, UP), w))
                .filter(|(b, _)| (center_y(*b) - center_y(r)).abs() <= (r.3 as i64).max(6) && b.0 + b.2 <= r.0 + 4)
                .filter(|(b, _)| b.0 + 600 >= r.0)
                .map(|(b, w)| (b.0, w.text.clone()))
                .collect();
            label.sort_by_key(|(x, _)| *x);
            let label: String = label.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join(" ");
            below.push((r, label));
        }
        below.sort_by_key(|(r, _)| r.1);
    }
    let sob = below
        .iter()
        .find(|(_, l)| {
            let l = l.to_lowercase();
            (l.contains("sum") && l.contains("best")) || l.contains("sob")
        })
        .map(|(r, _)| grow(*r, 14, 8, cw, ch));

    Ok(Some(Found {
        timer_ink,
        timer,
        split_rows,
        splits,
        row_pitch,
        counter,
        below,
        sob,
        time_words: times.iter().map(|(r, _)| *r).collect(),
    }))
}

fn fmt_rect(r: R) -> String {
    format!("{{ crop_x = {}, crop_y = {}, crop_w = {}, crop_h = {} }}", r.0, r.1, r.2, r.3)
}

fn report(f: &Found, gray: &GrayImage, cfg: &Config) -> Result<()> {
    let (cw, ch) = gray.dimensions();
    println!("LiveSplit pane found ({cw}x{ch} canvas coordinates)");
    println!(
        "  timer digits at x={} y={} w={} h={}  -> crop {}",
        f.timer_ink.0, f.timer_ink.1, f.timer_ink.2, f.timer_ink.3, fmt_rect(f.timer)
    );
    match (&f.splits, f.row_pitch) {
        (Some(s), pitch) => println!(
            "  split rows: {} read (of {} acts configured), row pitch {} px -> crop {}",
            f.split_rows.len(),
            cfg.game.acts.len(),
            pitch.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            fmt_rect(*s)
        ),
        (None, _) => println!("  split rows: none read above the timer"),
    }
    match f.counter {
        Some(c) => println!("  attempt counter -> crop {}", fmt_rect(c)),
        None => println!("  attempt counter: no bare integer found above the splits"),
    }
    for (r, label) in &f.below {
        println!("  row below timer at y={} labelled {label:?}", r.1);
    }
    match f.sob {
        Some(s) => println!("  sum of best -> crop {}", fmt_rect(s)),
        None => println!("  sum of best: no row labelled \"Sum of Best\" below the timer"),
    }

    println!("\n[[layouts]]");
    println!("name = \"found\"");
    println!("timer = {}", fmt_rect(f.timer));
    if let Some(s) = f.splits {
        println!("splits = {}", fmt_rect(s));
    }
    if let Some(c) = f.counter {
        println!("attempts_counter = {}", fmt_rect(c));
    }
    if let Some(s) = f.sob {
        println!("lifetime_sob = {}", fmt_rect(s));
    }

    // How far the pane sits from each configured layout, judged by the timer
    // digits: LiveSplit right-aligns them, so the right edge and the vertical
    // centre are the stable references.
    println!("\nAgainst configured layouts (timer right edge / vertical centre):");
    let ink_right = (f.timer_ink.0 + f.timer_ink.2) as i64;
    let ink_cy = center_y(f.timer_ink);
    for l in app::layout_rects(cfg) {
        let (x, y, w, h) = l.timer;
        let inside = f.timer_ink.0 >= x
            && f.timer_ink.1 >= y
            && f.timer_ink.0 + f.timer_ink.2 <= x + w
            && f.timer_ink.1 + f.timer_ink.3 <= y + h;
        let slack_l = f.timer_ink.0 as i64 - x as i64;
        let slack_r = (x + w) as i64 - ink_right;
        let slack_t = f.timer_ink.1 as i64 - y as i64;
        let slack_b = (y + h) as i64 - (f.timer_ink.1 + f.timer_ink.3) as i64;
        // Where the digits would sit if the crop were centred on them.
        let dx = ink_right + 12 - (x + w) as i64;
        let dy = ink_cy - (y as i64 + h as i64 / 2);
        println!(
            "  {:<12} {}  offset {dx:+},{dy:+} px   slack L{slack_l} R{slack_r} T{slack_t} B{slack_b}",
            l.name,
            if inside { "digits inside crop " } else { "digits CLIPPED     " }
        );
    }

    // Debug image: every time-shaped word grey, chosen crops coloured.
    let mut rgb: RgbImage = image::DynamicImage::ImageLuma8(gray.clone()).to_rgb8();
    for r in &f.time_words {
        draw_rect(&mut rgb, *r, Rgb([140, 140, 140]));
    }
    draw_rect(&mut rgb, f.timer_ink, Rgb([255, 120, 120]));
    draw_rect(&mut rgb, f.timer, Rgb([255, 0, 0]));
    if let Some(s) = f.splits {
        draw_rect(&mut rgb, s, Rgb([0, 220, 0]));
    }
    if let Some(c) = f.counter {
        draw_rect(&mut rgb, c, Rgb([60, 120, 255]));
    }
    if let Some(s) = f.sob {
        draw_rect(&mut rgb, s, Rgb([255, 220, 0]));
    }
    for l in app::layout_rects(cfg) {
        draw_rect(&mut rgb, l.timer, Rgb([255, 0, 255]));
    }
    let out = format!("{OUT_DIR}/locate.png");
    rgb.save(&out)?;
    println!("\nBoxes drawn in {out} (red = timer, green = splits, blue = counter, yellow = SoB, magenta = configured timer crops)");
    Ok(())
}

fn draw_rect(img: &mut RgbImage, r: R, color: Rgb<u8>) {
    let (w, h) = img.dimensions();
    let x1 = (r.0 + r.2).min(w.saturating_sub(1));
    let y1 = (r.1 + r.3).min(h.saturating_sub(1));
    for x in r.0.min(w - 1)..=x1 {
        for y in [r.1.min(h - 1), y1] {
            img.put_pixel(x, y, color);
        }
    }
    for y in r.1.min(h - 1)..=y1 {
        for x in [r.0.min(w - 1), x1] {
            img.put_pixel(x, y, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_and_counter_shapes() {
        assert!(is_time("1:22.47"));
        assert!(is_time("11:36.9"));
        assert!(is_time("58.2"));
        assert!(is_time("9.00.")); // trailing dot noise
        assert!(is_time("-0.3"));
        assert!(!is_time("95900"));
        assert!(!is_time("12"));
        assert!(is_counter("95900"));
        assert!(!is_counter("12"));
        assert!(!is_counter("1:22"));
    }

    #[test]
    fn rows_group_by_baseline() {
        let words = [(500, 100, 40, 18), (450, 102, 30, 18), (500, 142, 40, 18)];
        let mut rows: Vec<Vec<R>> = Vec::new();
        for r in words {
            match rows.last_mut() {
                Some(row) if (center_y(row[0]) - center_y(r)).abs() <= 9 => row.push(r),
                _ => rows.push(vec![r]),
            }
        }
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
    }
}
