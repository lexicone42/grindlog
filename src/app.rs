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

pub fn regions(cfg: &Config) -> Regions {
    let t = &cfg.timer;
    let mut rects: Vec<(u32, u32, u32, u32)> = vec![(t.crop_x, t.crop_y, t.crop_w, t.crop_h)];
    let s = &cfg.splits;
    if cfg.splits.enabled {
        rects.push((s.crop_x, s.crop_y, s.crop_w, s.crop_h));
    }
    let c = &cfg.attempts_counter;
    if c.enabled {
        rects.push((c.crop_x, c.crop_y, c.crop_w, c.crop_h));
    }
    let b = &cfg.lifetime_sob;
    if b.enabled {
        rects.push((b.crop_x, b.crop_y, b.crop_w, b.crop_h));
    }
    let ux = rects.iter().map(|r| r.0).min().unwrap();
    let uy = rects.iter().map(|r| r.1).min().unwrap();
    let uw = rects.iter().map(|r| r.0 + r.2).max().unwrap() - ux;
    let uh = rects.iter().map(|r| r.1 + r.3).max().unwrap() - uy;
    let rel = |r: &(u32, u32, u32, u32)| (r.0 - ux, r.1 - uy, r.2, r.3);
    Regions {
        union: (ux, uy, uw, uh),
        timer: rel(&rects[0]),
        splits: cfg
            .splits
            .enabled
            .then(|| rel(&(s.crop_x, s.crop_y, s.crop_w, s.crop_h))),
        counter: c
            .enabled
            .then(|| rel(&(c.crop_x, c.crop_y, c.crop_w, c.crop_h))),
        sob: b
            .enabled
            .then(|| rel(&(b.crop_x, b.crop_y, b.crop_w, b.crop_h))),
    }
}

pub fn capture_cfg(cfg: &Config) -> capture::CaptureCfg {
    let s = &cfg.stream;
    let (ux, uy, uw, uh) = regions(cfg).union;
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
        frame_len: (uw * uh) as usize,
        frame_timeout_secs: s.frame_timeout_secs,
        offline_poll_secs: s.offline_poll_secs,
        restart_delay_secs: s.restart_delay_secs,
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

    let reg = regions(&cfg);
    let (uw, uh) = (reg.union.2, reg.union.3);
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
    // (last stable counter value, consecutive sightings)
    let mut counter_stable: Option<(i64, u32)> = None;
    let mut last_counter_read_t: i64 = i64::MIN / 2;
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
        let (tx_, ty_, tw_, th_) = reg.timer;
        let gray = image::imageops::crop_imm(&union_img, tx_, ty_, tw_, th_).to_image();
        let processed = ocr::preprocess(&gray, &pre);
        let png = ocr::to_png(&processed)?;
        let text = match ocr_engine.recognize(&png).await {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                warn!("ocr failed: {e:#}");
                String::new()
            }
        };
        let parsed = parse_time(&text);
        let obs = parsed.map(Obs::Time).unwrap_or(Obs::Illegible);
        let t = if recorded {
            frame_idx * frame_interval_ms
        } else {
            epoch.elapsed().as_millis() as i64
        };
        frame_idx += 1;
        last_t = t;

        if session_id.is_none() {
            let started = time_base.map(|b| b + t).unwrap_or_else(util::unix_ms);
            match db::open_session(&pool, started, session_source, &session_label).await {
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
            if cr.ls_attempt.is_none() && t - last_counter_read_t >= splits_every_ms {
                last_counter_read_t = t;
                let cimg = image::imageops::crop_imm(&union_img, cx, cy, cw2, ch2).to_image();
                let cp = ocr::preprocess(&cimg, &pre_counter);
                if let Ok(txt) = ocr_engine.recognize(&ocr::to_png(&cp)?).await {
                    if let Some(v) = crate::timeparse::parse_counter(txt.trim()) {
                        counter_stable = match counter_stable {
                            Some((pv, n)) if pv == v => Some((v, n + 1)),
                            _ => Some((v, 1)),
                        };
                        if matches!(counter_stable, Some((_, n)) if n >= 2) {
                            info!("livesplit attempt counter: {v}");
                            cr.ls_attempt = Some(v);
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
                            info!("lifetime sum of best: {}", format_ms(v));
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
            let msg = if is_pb {
                format!(
                    "Run finished in {} — NEW {label} for {} [{}]! (attempt #{})",
                    format_ms(final_ms),
                    run.game,
                    run.category,
                    run.attempt_number
                )
            } else {
                format!(
                    "Run finished in {} ({} [{}], attempt #{}; {label} is {})",
                    format_ms(final_ms),
                    run.game,
                    run.category,
                    run.attempt_number,
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
