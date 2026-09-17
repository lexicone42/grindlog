//! `pane`: what the pane pass would read off one frame, for every layout —
//! the board as read (title, rows, cells), what `marathon::classify` calls
//! it, and the board probe's score at each threshold. This is the loop that
//! took twelve replay trials against his stream on 2026-09-17, done by hand
//! with a debug line each time: which layout's rectangle reads the rows,
//! at which threshold the names come through, and whether the verdict is
//! the board's. Ten seconds now.
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
) -> Result<()> {
    let (cw, ch) = (cfg.stream.canvas_w, cfg.stream.canvas_h);
    let gray = match image {
        Some(path) => {
            let img = image::open(&path).with_context(|| format!("opening {}", path.display()))?;
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
    println!(
        "union {},{} {}x{} (canvas); pane rectangles per layout, then rows where three or more read",
        u.0, u.1, u.2, u.3
    );
    for thr in thresholds {
        println!("--- threshold {thr}");
        for (li, r) in regs.iter().enumerate() {
            if layout.as_deref().is_some_and(|l| l != names[li]) {
                continue;
            }
            let (b, pane) = read(&mut engine, &union_img, r, thr, &cfg).await?;
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
) -> Result<(board::Board, app::R)> {
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
    Ok((board::read_board(&words, &letters, up, r.timer), pane))
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
