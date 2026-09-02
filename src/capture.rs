//! Video capture: turns a live Twitch stream into a steady sequence of raw
//! frames on a channel, restarting with backoff when the stream drops and
//! going dormant (slow polling) when the channel is offline.
//!
//! Frames come out of ffmpeg as fixed-size rawvideo buffers (the filter chain
//! does fps limiting, canvas scaling and cropping), so framing the byte
//! stream is just `read_exact(frame_len)`.

use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::SourceKind;
use crate::twitch_hls::{self, Resolved};

#[derive(Debug, Clone)]
pub struct CaptureCfg {
    pub channel: String,
    pub quality: String,
    pub source: SourceKind,
    pub streamlink_extra_args: Vec<String>,
    /// ffmpeg -vf filter chain (fps, scale, optional crop).
    pub filter: String,
    /// ffmpeg output pixel format ("gray" or "rgb24").
    pub pix_fmt: String,
    /// Only capture live streams whose title contains this (case-insensitive).
    pub title_filter: Option<String>,
    /// Twitch VOD id for SourceKind::Vod.
    pub vod_id: String,
    /// Local path / URL for SourceKind::File.
    pub input: String,
    /// Seek this far into a recording before decoding (Vod/File only).
    pub start_secs: f64,
    /// Exact byte length of one output frame.
    pub frame_len: usize,
    pub frame_timeout_secs: u64,
    pub offline_poll_secs: u64,
    pub restart_delay_secs: u64,
    /// Local minutes-of-day window when the streamer is expected; outside it
    /// offline polling slows to quiet_poll_secs.
    pub active_window: Option<(u32, u32)>,
    pub quiet_poll_secs: u64,
}

impl CaptureCfg {
    /// Offline poll interval for right now: frequent inside the expected
    /// window (and for a few minutes before it), quiet otherwise.
    fn current_offline_poll(&self) -> u64 {
        let Some((start, end)) = self.active_window else {
            return self.offline_poll_secs;
        };
        let now = chrono::Local::now();
        let m = now.format("%H").to_string().parse::<u32>().unwrap_or(0) * 60
            + now.format("%M").to_string().parse::<u32>().unwrap_or(0);
        let lead = start.saturating_sub(10); // wake up a little early
        let inside = if lead <= end { (lead..=end).contains(&m) } else { m >= lead || m <= end };
        if inside {
            self.offline_poll_secs
        } else {
            // Don't sleep past the window start.
            let mins_to_start = if m < lead { lead - m } else { 24 * 60 - m + lead };
            self.quiet_poll_secs.min(u64::from(mins_to_start) * 60).max(60)
        }
    }
}

/// What the capture layer hands downstream.
pub enum CaptureEvent {
    /// One raw video frame (exactly frame_len bytes).
    Frame(Vec<u8>),
    /// The channel is confirmed offline (broadcast over) — sent when the
    /// loop enters its dormant polling state. Mid-stream hiccups/restarts do
    /// NOT produce this.
    StreamOffline,
}

enum Session {
    Offline,
    ReceiverGone,
    Ended { frames: u64 },
}

/// Runs forever (until the frame receiver is dropped), producing frames.
pub async fn capture_loop(cfg: CaptureCfg, tx: mpsc::Sender<CaptureEvent>) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) ngtwitchtimer/0.1")
        .build()
        .context("building http client")?;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        match run_once(&cfg, &http, &tx).await {
            Ok(Session::ReceiverGone) => return Ok(()),
            // Recorded input ran to its end: we're done, don't restart.
            Ok(Session::Ended { frames }) if cfg.source.is_recorded() => {
                info!("recorded input finished after {frames} frames");
                return Ok(());
            }
            Ok(Session::Offline) if cfg.source.is_recorded() => {
                anyhow::bail!("vod {} not found or not accessible", cfg.vod_id);
            }
            Ok(Session::Offline) => {
                let wait = cfg.current_offline_poll();
                info!("{} is offline; checking again in {wait}s", cfg.channel);
                let _ = tx.send(CaptureEvent::StreamOffline).await;
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            Ok(Session::Ended { frames }) => {
                // A session that never really produced frames is an offline
                // signal in streamlink mode (in hls mode the resolver already
                // gates on liveness, so a quick death is just a hiccup).
                let offline_ish = cfg.source == SourceKind::Streamlink && frames < 5;
                let delay = if offline_ish {
                    let _ = tx.send(CaptureEvent::StreamOffline).await;
                    cfg.offline_poll_secs
                } else {
                    cfg.restart_delay_secs
                };
                info!(
                    "capture session ended after {frames} frames; restarting in {delay}s"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            Err(e) => {
                warn!("capture error: {e:#}; retrying in {}s", cfg.restart_delay_secs);
                tokio::time::sleep(Duration::from_secs(cfg.restart_delay_secs)).await;
            }
        }
    }
}

async fn run_once(
    cfg: &CaptureCfg,
    http: &reqwest::Client,
    tx: &mpsc::Sender<CaptureEvent>,
) -> Result<Session> {
    let mut streamlink: Option<Child> = None;
    let mut ff = Command::new("ffmpeg");
    ff.args(["-hide_banner", "-loglevel", "warning"]);

    match cfg.source {
        SourceKind::Hls => {
            if let Some(filter) = cfg.title_filter.as_deref().filter(|f| !f.trim().is_empty()) {
                match twitch_hls::stream_title(http, &cfg.channel).await? {
                    Some(title)
                        if !title.to_lowercase().contains(&filter.trim().to_lowercase()) =>
                    {
                        info!(
                            "{} is live but streaming something else ({title:?}); waiting",
                            cfg.channel
                        );
                        return Ok(Session::Offline);
                    }
                    _ => {}
                }
            }
            match twitch_hls::resolve(http, &cfg.channel, &cfg.quality).await? {
                Resolved::Offline => return Ok(Session::Offline),
                Resolved::Live { variant_url, name } => {
                    info!("{} is live ({name}); starting ffmpeg", cfg.channel);
                    // -rw_timeout makes ffmpeg give up on a stalled socket
                    // instead of hanging forever (microseconds).
                    ff.args(["-rw_timeout", "15000000", "-i", &variant_url]);
                    ff.stdin(Stdio::null());
                }
            }
        }
        SourceKind::Vod => {
            match twitch_hls::resolve_vod(http, &cfg.vod_id, &cfg.quality).await? {
                Resolved::Offline => return Ok(Session::Offline),
                Resolved::Live { variant_url, name } => {
                    info!("vod {} resolved ({name}); starting ffmpeg", cfg.vod_id);
                    ff.args(["-rw_timeout", "15000000"]);
                    seek_args(&mut ff, cfg.start_secs);
                    ff.args(["-i", &variant_url]);
                    ff.stdin(Stdio::null());
                }
            }
        }
        SourceKind::File => {
            info!("reading {}", cfg.input);
            seek_args(&mut ff, cfg.start_secs);
            ff.args(["-i", &cfg.input]);
            ff.stdin(Stdio::null());
        }
        SourceKind::Streamlink => {
            let mut sl = Command::new("streamlink");
            sl.arg("--stdout");
            sl.args(&cfg.streamlink_extra_args);
            sl.arg(format!("twitch.tv/{}", cfg.channel));
            sl.arg(&cfg.quality);
            sl.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = sl.spawn().context(
                "failed to spawn streamlink — install it or set stream.source = \"hls\"",
            )?;
            drain_stderr("streamlink", child.stderr.take());
            let sl_out = child.stdout.take().expect("piped stdout");
            streamlink = Some(child);
            ff.args(["-i", "pipe:0"]);
            ff.stdin(Stdio::piped());
            // The copy task is wired up after ffmpeg spawns.
            return finish_pipeline(cfg, ff, streamlink, Some(sl_out), tx).await;
        }
    }
    finish_pipeline(cfg, ff, streamlink, None, tx).await
}

/// Input-side seek (`-ss` before `-i`): ffmpeg skips whole HLS segments /
/// keyframes to get there, so a deep seek into a VOD is nearly free.
fn seek_args(ff: &mut Command, start_secs: f64) {
    if start_secs > 0.0 {
        ff.args(["-ss", &format!("{start_secs:.3}")]);
    }
}

async fn finish_pipeline(
    cfg: &CaptureCfg,
    mut ff: Command,
    mut streamlink: Option<Child>,
    sl_out: Option<tokio::process::ChildStdout>,
    tx: &mpsc::Sender<CaptureEvent>,
) -> Result<Session> {
    ff.args(["-an", "-sn"])
        .args(["-vf", &cfg.filter])
        .args(["-f", "rawvideo", "-pix_fmt", &cfg.pix_fmt, "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut ffmpeg = ff.spawn().context("failed to spawn ffmpeg")?;
    drain_stderr("ffmpeg", ffmpeg.stderr.take());

    if let Some(mut sl_out) = sl_out {
        let mut ff_in = ffmpeg.stdin.take().expect("piped stdin");
        tokio::spawn(async move {
            // EOF (or ffmpeg death) ends the copy; dropping ff_in closes
            // ffmpeg's stdin so it drains and exits.
            let _ = tokio::io::copy(&mut sl_out, &mut ff_in).await;
        });
    }

    let mut out = ffmpeg.stdout.take().expect("piped stdout");
    let timeout = Duration::from_secs(cfg.frame_timeout_secs);
    let mut frames: u64 = 0;
    let mut receiver_gone = false;
    loop {
        let mut buf = vec![0u8; cfg.frame_len];
        match tokio::time::timeout(timeout, out.read_exact(&mut buf)).await {
            Err(_) => {
                warn!(
                    "no frame for {}s; restarting the pipeline",
                    cfg.frame_timeout_secs
                );
                break;
            }
            Ok(Err(_)) => break, // EOF: stream ended or ffmpeg died
            Ok(Ok(_)) => {
                frames += 1;
                if tx.send(CaptureEvent::Frame(buf)).await.is_err() {
                    receiver_gone = true;
                    break;
                }
            }
        }
    }

    let _ = ffmpeg.kill().await;
    if let Some(mut sl) = streamlink.take() {
        let _ = sl.kill().await;
    }
    if receiver_gone {
        Ok(Session::ReceiverGone)
    } else {
        Ok(Session::Ended { frames })
    }
}

fn drain_stderr(name: &'static str, stderr: Option<tokio::process::ChildStderr>) {
    let Some(stderr) = stderr else { return };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(target: "capture", "{name}: {line}");
        }
    });
}
