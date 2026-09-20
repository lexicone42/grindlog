//! `pane`: what the pane pass would read off one frame, for every layout —
//! the board as read (title, rows, cells), what `marathon::classify` calls
//! it, and the board probe's score at each threshold: which layout's
//! rectangle reads the rows, at which threshold the names come through, and
//! whether the verdict is the board's — in seconds, where a replay trial per
//! question takes minutes.
//!
//! Reads a PNG (a canvas-scaled frame, `calibrate --full-frame` saves one)
//! or grabs one frame from the configured source, crops the decoded union
//! exactly as the bot does, and for each layout OCRs that layout's own pane
//! rectangle (`app::pane_rect`) at the configured `[splits] threshold` and
//! at the board probe's alternatives.
use crate::app::{self, Regions};
use crate::board;
use crate::config::Config;
use crate::marathon;
use crate::ocr::{self, OcrEngine, PreprocessCfg};
use anyhow::{bail, Context, Result};
use image::GrayImage;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::error;

pub async fn run(
    cfg: Config,
    image: Option<PathBuf>,
    thresholds: Vec<u8>,
    layout: Option<String>,
    dump_fixture: Option<PathBuf>,
) -> Result<()> {
    if dump_fixture.is_some() && layout.is_none() {
        bail!("--dump-fixture needs --layout: a fixture is one layout's pane");
    }
    let (cw, ch) = (cfg.stream.canvas_w, cfg.stream.canvas_h);
    let gray = match &image {
        Some(path) => {
            let img = image::open(path).with_context(|| format!("opening {}", path.display()))?;
            let mut g = img.to_luma8();
            if g.dimensions() != (cw, ch) {
                g = image::imageops::resize(&g, cw, ch, image::imageops::FilterType::Triangle);
            }
            g
        }
        None => grab_frame(&cfg).await?,
    };
    let regs = app::regions(&cfg);
    let names = app::layout_names(&cfg);
    let u = regs[0].union;
    let union_img = image::imageops::crop_imm(&gray, u.0, u.1, u.2, u.3).to_image();
    let mut engine = OcrEngine::from_config(&cfg.ocr)?;
    let mut thresholds = if thresholds.is_empty() {
        vec![cfg.splits.threshold, 100]
    } else {
        thresholds
    };
    thresholds.dedup();
    let first = thresholds[0];
    println!(
        "union {},{} {}x{} (canvas); pane rectangles per layout, then rows where three or more read",
        u.0, u.1, u.2, u.3
    );
    for thr in thresholds.iter().copied() {
        println!("--- threshold {thr}");
        for (li, r) in regs.iter().enumerate() {
            if layout.as_deref().is_some_and(|l| l != names[li]) {
                continue;
            }
            let (b, pane, words, letters) = read(&mut engine, &union_img, r, thr, &cfg).await?;
            if let Some(path) = dump_fixture.as_ref().filter(|_| thr == first) {
                write_fixture(
                    path,
                    &cfg,
                    &names[li],
                    u,
                    pane,
                    r,
                    &words,
                    &letters,
                    &b,
                    image.as_deref(),
                )?;
                println!(
                    "fixture written to {} — fill in `expected` by hand",
                    path.display()
                );
            }
            let named = b.rows.iter().filter(|x| x.name.is_some()).count();
            let verdict = match marathon::classify(&b, &cfg, None) {
                marathon::Verdict::Board(a) => format!("board-mode {:?}", a.name),
                marathon::Verdict::Other => "other".to_string(),
                marathon::Verdict::Silent => "silent".to_string(),
            };
            println!(
                "{:<12} pane {},{} {}x{}  rows {:>2} named {:>2}  title {:?} [{:?}] counter {:?}  {}",
                names[li],
                u.0 + pane.0,
                u.1 + pane.1,
                pane.2,
                pane.3,
                b.rows.len(),
                named,
                b.title.as_deref().unwrap_or("-"),
                b.subtitle.as_deref().unwrap_or("-"),
                b.counter.as_deref().unwrap_or("-"),
                verdict
            );
            if b.rows.len() >= 3 {
                for row in &b.rows {
                    println!(
                        "    y={:>4} {:<34} {}",
                        row.y,
                        row.name.as_deref().unwrap_or("?"),
                        row.cells.join(" / ")
                    );
                }
            }
        }
    }
    Ok(())
}

/// The pane pass for one layout at one threshold, as `app::measure_pane`
/// does it: this layout's pane rectangle alone, upscaled, both OCR passes,
/// the words shifted back into the union's pixels, and the board read over
/// the layout's timer rectangle.
async fn read(
    engine: &mut OcrEngine,
    union_img: &GrayImage,
    r: &Regions,
    threshold: u8,
    cfg: &Config,
) -> Result<(board::Board, app::R, Vec<ocr::Word>, Vec<ocr::Word>)> {
    let up = app::PANE_UP;
    let pane = app::pane_rect(r, union_img.width(), union_img.height());
    let sub = image::imageops::crop_imm(union_img, pane.0, pane.1, pane.2, pane.3).to_image();
    let pre = PreprocessCfg {
        upscale: up,
        threshold,
        invert: cfg.splits.invert,
        auto_threshold: false,
    };
    let proc = ocr::preprocess(&sub, &pre);
    let png = ocr::to_png(&proc)?;
    let shift = |mut w: ocr::Word| {
        w.x += pane.0 * up;
        w.y += pane.1 * up;
        w
    };
    let words: Vec<ocr::Word> = engine
        .recognize_words(&png, Some("0123456789:."), 11)
        .await?
        .into_iter()
        .map(shift)
        .collect();
    let letters: Vec<ocr::Word> = engine
        .recognize_words(&png, None, 11)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(shift)
        .collect();
    let board = board::read_board(&words, &letters, up, r.timer);
    Ok((board, pane, words, letters))
}

/// A board-reader fixture in the shape tests/fixtures/board/README.md
/// describes: the words in the PANE's pixels (the fixture's crop is the
/// pane rectangle, in canvas coordinates) and the timer rectangle relative
/// to it, with an `expected` block copied from what was read, to be
/// corrected by hand against the frame before it becomes a test.
#[allow(clippy::too_many_arguments)]
fn write_fixture(
    path: &std::path::Path,
    cfg: &Config,
    layout: &str,
    union: app::R,
    pane: app::R,
    r: &Regions,
    words: &[ocr::Word],
    letters: &[ocr::Word],
    board: &board::Board,
    image: Option<&std::path::Path>,
) -> Result<()> {
    let up = app::PANE_UP;
    let unshift = |w: &ocr::Word| {
        serde_json::json!({
            "x": w.x - pane.0 * up, "y": w.y - pane.1 * up, "w": w.w, "h": w.h,
            "conf": w.conf, "text": w.text,
        })
    };
    let fixture = serde_json::json!({
        "name": path.file_stem().and_then(|s| s.to_str()).unwrap_or("pane"),
        "source": image.map(|p| p.display().to_string()).unwrap_or_else(|| format!("a frame of {}", cfg.stream.channel)),
        "scale": up,
        "crop": {
            "x": union.0 + pane.0, "y": union.1 + pane.1, "w": pane.2, "h": pane.3,
            "frame_w": cfg.stream.canvas_w, "frame_h": cfg.stream.canvas_h,
        },
        "timer": [r.timer.0 - pane.0, r.timer.1 - pane.1, r.timer.2, r.timer.3],
        "words": words.iter().map(unshift).collect::<Vec<_>>(),
        "letters": letters.iter().map(unshift).collect::<Vec<_>>(),
        "expected": {
            "title": board.title, "subtitle": board.subtitle, "counter": board.counter,
            "rows": board.rows.iter().map(|row| serde_json::json!({"name": row.name, "cells": row.cells})).collect::<Vec<_>>(),
        },
        "notes": format!("layout {layout}: AS READ — correct `expected` against the frame before this is a test"),
    });
    std::fs::write(path, serde_json::to_string_pretty(&fixture)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// One whole canvas-scaled frame from the configured source, as `locate`
/// takes it.
async fn grab_frame(cfg: &Config) -> Result<GrayImage> {
    let (cw, ch) = (cfg.stream.canvas_w, cfg.stream.canvas_h);
    let mut cap = app::capture_cfg(cfg);
    cap.filter = format!("fps={},scale={}:{}:flags=bicubic", cfg.stream.fps, cw, ch);
    cap.pix_fmt = "gray".into();
    cap.frame_len = (cw * ch) as usize;
    let (tx, mut rx) = mpsc::channel::<crate::capture::CaptureEvent>(4);
    tokio::spawn(async move {
        if let Err(e) = crate::capture::capture_loop(cap, tx).await {
            error!("capture loop died: {e:#}");
        }
    });
    while let Some(ev) = rx.recv().await {
        match ev {
            crate::capture::CaptureEvent::Frame(raw) => {
                if let Some(g) = GrayImage::from_raw(cw, ch, raw) {
                    return Ok(g);
                }
            }
            crate::capture::CaptureEvent::StreamOffline => {
                bail!("{} is offline; pass --image instead", cfg.stream.channel)
            }
        }
    }
    bail!("input ended before a frame arrived")
}
