//! Calibration mode: saves what the OCR pipeline actually sees so the crop
//! rectangle and threshold can be tuned, and prints live OCR readings.
//!
//! `calibrate --full-frame` captures the whole canvas-scaled frame instead
//! (color), for finding crop coordinates in an image viewer.

use anyhow::{Context, Result};
use image::{GrayImage, RgbImage};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::config::Config;
use crate::ocr::{self, OcrEngine, PreprocessCfg};
use crate::timeparse::{format_ms, parse_time};
use crate::{app, capture};

const OUT_DIR: &str = "calibration";

pub async fn run(cfg: Config, full_frame: bool) -> Result<()> {
    std::fs::create_dir_all(OUT_DIR).context("creating calibration/ dir")?;

    let mut cap = app::capture_cfg(&cfg);
    if full_frame {
        let s = &cfg.stream;
        cap.filter = format!(
            "fps={},scale={}:{}:flags=bicubic",
            s.fps, s.canvas_w, s.canvas_h
        );
        cap.pix_fmt = "rgb24".into();
        cap.frame_len = (s.canvas_w * s.canvas_h * 3) as usize;
    }

    let mut engine = if full_frame {
        None
    } else {
        match OcrEngine::from_config(&cfg.ocr) {
            Ok(e) => Some(e),
            Err(e) => {
                warn!("OCR unavailable ({e:#}); saving crop images without readings");
                None
            }
        }
    };
    let pre = PreprocessCfg::from(&cfg.timer);

    let (tx, mut rx) = mpsc::channel::<capture::CaptureEvent>(4);
    tokio::spawn(async move {
        if let Err(e) = capture::capture_loop(cap, tx).await {
            error!("capture loop died: {e:#}");
        }
    });

    if full_frame {
        println!(
            "Full-frame mode: each frame overwrites {OUT_DIR}/full.png \
             ({}x{} canvas). Open it and note the timer's x/y/w/h for [timer] \
             in the config. Ctrl+C to stop.",
            cfg.stream.canvas_w, cfg.stream.canvas_h
        );
    } else {
        println!(
            "Crop mode: each frame overwrites {OUT_DIR}/crop.png (raw) and \
             {OUT_DIR}/processed.png (what tesseract sees). Live readings below \
             — tune [timer] until they parse cleanly. Ctrl+C to stop.",
        );
    }

    let mut n: u64 = 0;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let raw = tokio::select! {
            _ = &mut ctrl_c => break,
            maybe = rx.recv() => match maybe {
                Some(capture::CaptureEvent::Frame(raw)) => raw,
                Some(capture::CaptureEvent::StreamOffline) => continue,
                None => break,
            },
        };
        n += 1;
        if full_frame {
            let (w, h) = (cfg.stream.canvas_w, cfg.stream.canvas_h);
            let Some(img) = RgbImage::from_raw(w, h, raw) else {
                continue;
            };
            img.save(format!("{OUT_DIR}/full.png"))?;
            println!("[{n:04}] saved {OUT_DIR}/full.png");
        } else {
            // Frames arrive as the union crop of all regions/layouts; cut the
            // base layout's timer rectangle out of it.
            let reg = &app::regions(&cfg)[0];
            let (uw, uh) = (reg.union.2, reg.union.3);
            // The union arrives in colour; the timer is read from the
            // brightest channel, exactly as the bot does it.
            let Some(union_rgb) = RgbImage::from_raw(uw, uh, raw) else {
                continue;
            };
            let (tx, ty, tw, th) = reg.timer;
            let crop = image::imageops::crop_imm(&union_rgb, tx, ty, tw, th).to_image();
            let gray = GrayImage::from_fn(tw, th, |x, y| {
                let p = crop.get_pixel(x, y).0;
                image::Luma([p[0].max(p[1]).max(p[2])])
            });
            gray.save(format!("{OUT_DIR}/crop.png"))?;
            let processed = ocr::preprocess(&gray, &pre);
            processed.save(format!("{OUT_DIR}/processed.png"))?;
            match engine.as_mut() {
                None => println!("[{n:04}] saved crop.png/processed.png (no OCR engine)"),
                Some(engine) => {
                    let text = engine
                        .recognize(&ocr::to_png(&processed)?)
                        .await
                        .unwrap_or_else(|e| format!("<ocr error: {e}>"));
                    let text = text.trim();
                    match parse_time(text) {
                        Some(ms) => println!("[{n:04}] ocr={text:?} -> {}", format_ms(ms)),
                        None => println!("[{n:04}] ocr={text:?} -> unparseable"),
                    }
                }
            }
        }
    }
    Ok(())
}
