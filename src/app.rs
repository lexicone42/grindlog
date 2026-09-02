//! The main run mode: capture -> preprocess -> OCR -> state machine -> DB/chat.

use anyhow::{Context, Result};
use image::GrayImage;
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
    pub sob: Option<(u32, u32, u32, u32)>,     // relative to union
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
        splits: s.enabled.then_some((s.crop_x, s.crop_y, s.crop_w, s.crop_h)),
        counter: c.enabled.then_some((c.crop_x, c.crop_y, c.crop_w, c.crop_h)),
        sob: b.enabled.then_some((b.crop_x, b.crop_y, b.crop_w, b.crop_h)),
    };
    let rect = |r: crate::config::Rect| (r.crop_x, r.crop_y, r.crop_w, r.crop_h);
    let mut all = vec![base];
    for l in &cfg.layouts {
        let base = &all[0];
        all.push(LayoutRects {
            name: l.name.clone(),
            timer: rect(l.timer),
            splits: base.splits.map(|d| l.splits.map(rect).unwrap_or(d)),
            counter: base.counter.map(|d| l.attempts_counter.map(rect).unwrap_or(d)),
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
    // Pad the union by the drift-search margin (clamped to the canvas) so
    // offset probes stay inside the decoded frame.
    let pad = cfg.layout_search.drift_px;
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
/// ink's right edge and vertical centre in un-upscaled crop pixels. LiveSplit
/// right-aligns the timer, so both stay put while the window does; a shift
/// means the window moved. Rows/columns with only a few ink pixels are noise.
fn ink_anchor(proc: &GrayImage, upscale: u32) -> Option<(i32, i32)> {
    let up = upscale.max(1);
    let (w, h) = proc.dimensions();
    let min = 3 * up;
    let mut cols = vec![0u32; w as usize];
    for (x, _, p) in proc.enumerate_pixels() {
        if p.0[0] == 0 {
            cols[x as usize] += 1;
        }
    }
    // A pane border or the game area's edge inside the crop is ink spanning
    // (nearly) the full crop height; digits never do. Drop such columns.
    let digit_col = |c: u32| c >= min && c <= h * 9 / 10;
    let mut rows = vec![0u32; h as usize];
    for (x, y, p) in proc.enumerate_pixels() {
        if p.0[0] == 0 && digit_col(cols[x as usize]) {
            rows[y as usize] += 1;
        }
    }
    let right = cols.iter().rposition(|&c| digit_col(c))? as f32;
    let top = rows.iter().position(|&c| c >= min)? as f32;
    let bottom = rows.iter().rposition(|&c| c >= min)? as f32;
    let up = up as f32;
    Some(((right / up).round() as i32, ((top + bottom) / 2.0 / up).round() as i32))
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
        pix_fmt: "gray".into(),
        title_filter: s.title_filter.clone(),
        vod_id: s.vod_id.clone(),
        input: s.input.clone(),
        start_secs: s.start_secs,
        frame_len: (uw * uh) as usize,
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
            cfg.chat.channel.trim().trim_start_matches('#').to_ascii_lowercase()
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
        cands.push(Candidate { layout: i, off: (0, 0), regs: r.clone(), streak: 0, last: None });
        for &off in &offsets {
            if let Some(sr) = shifted(r, off) {
                cands.push(Candidate { layout: i, off, regs: sr, streak: 0, last: None });
            }
        }
    }
    let mut active_layout: usize = 0;
    let mut active_regs: Regions = regs[0].clone();
    let mut active_off: (i32, i32) = (0, 0);
    let mut layout_locked = cands.len() == 1;
    let mut probe_rr: usize = 0;
    let mut dark_frames: u32 = 0;
    let dark_frames_search = cfg.layout_search.dark_frames_search.max(1);
    // Fine drift while locked: where the digits sit inside each layout's
    // timer crop (learned on first lock), and the recent shifts seen from it.
    let mut anchors = vec![None::<(i32, i32)>; regs.len()];
    let mut drift_hits: Vec<(i32, i32)> = Vec::new();
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
                        &pool, &shared, &announce_tx, announce, &mut current, ev, wall_now,
                    )
                    .await
                    {
                        warn!("failed to record end-of-stream reset: {e:#}");
                    }
                    tracker = Tracker::new(cfg.detection.clone());
                }
                if let Some(id) = session_id.take() {
                    if let Err(e) = db::close_session(&pool, id, wall_now).await {
                        warn!("failed to close session {id}: {e:#}");
                    } else {
                        info!("session #{id} closed");
                    }
                }
                continue;
            }
        };

        let Some(union_img) = GrayImage::from_raw(uw, uh, raw) else {
            warn!("dropped a frame with unexpected size");
            continue;
        };
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
        if layout_locked {
            let r = active_regs.timer;
            let g = image::imageops::crop_imm(&union_img, r.0, r.1, r.2, r.3).to_image();
            let proc = ocr::preprocess(&g, &pre);
            match ocr_engine.recognize(&ocr::to_png(&proc)?).await {
                Ok(t) => text = t.trim().to_string(),
                Err(e) => warn!("ocr failed: {e:#}"),
            }
            if fine_drift && parse_time(&text).is_some() {
                ink = ink_anchor(&proc, pre.upscale);
            }
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
            // One offset candidate per layout takes its turn this frame.
            let n_off = offsets.len().max(1);
            let turn = probe_rr % n_off;
            probe_rr = probe_rr.wrapping_add(1);
            for li in 0..regs.len() {
                let pick = cands
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.layout == li && c.off != (0, 0))
                    .nth(turn)
                    .map(|(ci, _)| ci);
                if let Some(ci) = pick {
                    if !to_read.contains(&ci) {
                        to_read.push(ci);
                    }
                }
            }
            let mut winner: Option<(usize, String)> = None;
            for ci in to_read {
                let c = &mut cands[ci];
                let rd = match ocr_engine.recognize(&read_timer(&union_img, c.regs.timer)?).await {
                    Ok(t) => t.trim().to_string(),
                    Err(_) => String::new(),
                };
                let v = parse_time(&rd);
                if c.observe(v, t) && c.streak >= 5 && winner.is_none() {
                    winner = Some((ci, rd.clone()));
                }
                if c.layout == active_layout && c.off == active_off {
                    text = rd;
                }
            }
            if let Some((ci, winner_text)) = winner {
                text = winner_text;
                let c = &cands[ci];
                let name = &layout_names[c.layout];
                if c.layout != active_layout {
                    info!("layout switched to {name:?} (offset {:+},{:+})", c.off.0, c.off.1);
                    // A layout change invalidates any splits baseline.
                    splits_tracker = None;
                } else if c.off != active_off {
                    info!(
                        "layout {name:?} re-anchored: LiveSplit moved {:+},{:+} px (was {:+},{:+})",
                        c.off.0, c.off.1, active_off.0, active_off.1
                    );
                } else {
                    info!("layout locked: {name:?} (offset {:+},{:+})", c.off.0, c.off.1);
                }
                active_layout = c.layout;
                active_off = c.off;
                active_regs = c.regs.clone();
                layout_locked = true;
                drift_hits.clear();
                for c in cands.iter_mut() {
                    c.streak = 0;
                    c.last = None;
                }
            }
        }
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
                None => anchors[active_layout] = Some(m),
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
                            Some(nr) => {
                                info!(
                                    "layout {:?} re-anchored: LiveSplit moved {:+},{:+} px (now {:+},{:+} from configured)",
                                    layout_names[active_layout], d.0, d.1, new_off.0, new_off.1
                                );
                                active_off = new_off;
                                active_regs = nr;
                            }
                            None => warn!(
                                "LiveSplit moved {:+},{:+} px, beyond layout_search.drift_px; crops stay put",
                                d.0, d.1
                            ),
                        }
                        drift_hits.clear();
                    }
                }
            }
        }
        let reg = &active_regs;
        let obs = parsed.map(Obs::Time).unwrap_or(Obs::Illegible);
        last_t = t;

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
        if let (Some(st), Some((sx, sy, sw, sh))) = (splits_tracker.as_mut(), reg.splits) {
            if t - last_splits_read_t >= splits_every_ms {
                last_splits_read_t = t;
                let panel = image::imageops::crop_imm(&union_img, sx, sy, sw, sh).to_image();
                let rows = shared.acts.len().max(1) as u32;
                let row_h = (sh / rows).max(1);
                let mut values = Vec::with_capacity(rows as usize);
                for i in 0..rows {
                    let row =
                        image::imageops::crop_imm(&panel, 0, i * row_h, sw, row_h).to_image();
                    let rp = ocr::preprocess(&row, &pre_splits);
                    let txt = match ocr_engine.recognize(&ocr::to_png(&rp)?).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("splits ocr failed: {e:#}");
                            String::new()
                        }
                    };
                    values.push(parse_time(txt.trim()));
                }
                for (idx, cum) in st.observe(&values, tracker.smoothed_now(t)) {
                    let act_name = shared
                        .acts
                        .get(idx)
                        .map(|a| a.0.clone())
                        .unwrap_or_else(|| format!("Act {}", idx + 1));
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
                let cimg = image::imageops::crop_imm(&union_img, cx, cy, cw2, ch2).to_image();
                let cp = ocr::preprocess(&cimg, &pre_counter);
                if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&cp)?).await {
                    if let Some(v) = crate::timeparse::parse_counter(txt.trim())
                        .filter(|&v| last_ls_attempt.map(|p| v > p && v < p + 500).unwrap_or(true))
                    {
                        counter_stable = match counter_stable {
                            Some((pv, n)) if pv == v => Some((v, n + 1)),
                            _ => Some((v, 1)),
                        };
                        if matches!(counter_stable, Some((_, n)) if n >= 2) {
                            info!("livesplit attempt counter: {v}");
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
                let bimg = image::imageops::crop_imm(&union_img, bx, by, bw2, bh2).to_image();
                let bp = ocr::preprocess(&bimg, &pre_counter);
                if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&bp)?).await {
                    // Plausibility: a Sum of Best can never exceed the record
                    // (and can't be wildly below it) — this rejects consistent
                    // misreads when the layout shifts under the crop.
                    let bound = shared.baseline_best_ms.unwrap_or(i64::MAX);
                    if let Some(v) = parse_time(txt.trim())
                        .filter(|&v| v <= bound && v > bound / 2)
                    {
                        sob_stable = match sob_stable {
                            Some((pv, n)) if pv == v => Some((v, n + 1)),
                            _ => Some((v, 1)),
                        };
                        if matches!(sob_stable, Some((_, n)) if n >= 2)
                            && sob_recorded != Some(v)
                        {
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
        }

        let wall_now = time_base.map(|b| b + t).unwrap_or_else(util::unix_ms);
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
            if let Err(e) =
                handle_event(&pool, &shared, &announce_tx, announce, &mut current, ev, wall_now)
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
        let offs = drift_offsets(&LayoutSearchCfg { drift_px: 24, step_px: 12, dark_frames_search: 30 });
        assert_eq!(offs.len(), 24);
        assert!(!offs.contains(&(0, 0)));
        // First ring (Chebyshev distance 12) precedes the second (24); axis
        // neighbours come before diagonals within a ring.
        assert_eq!(offs[0].0.abs().max(offs[0].1.abs()), 12);
        assert!(offs[..8].iter().all(|&(x, y)| x.abs().max(y.abs()) == 12));
        assert!(offs[8..].iter().all(|&(x, y)| x.abs().max(y.abs()) == 24));
        assert!(offs[..4].iter().all(|&(x, y)| x == 0 || y == 0));
        assert!(drift_offsets(&LayoutSearchCfg { drift_px: 0, step_px: 12, dark_frames_search: 30 }).is_empty());
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

    #[test]
    fn candidate_accepts_frozen_or_advancing_readings() {
        let mut c = Candidate { layout: 0, off: (0, 0), regs: regs(), streak: 0, last: None };
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
