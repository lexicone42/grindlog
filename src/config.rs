//! TOML configuration. Every field except `stream.channel` has a sensible
//! default, so a minimal config is just:
//!
//! ```toml
//! [stream]
//! channel = "somestreamer"
//! ```

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::state::TrackerConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub stream: StreamCfg,
    #[serde(default)]
    pub timer: TimerCfg,
    #[serde(default)]
    pub ocr: OcrCfg,
    #[serde(default)]
    pub detection: TrackerConfig,
    #[serde(default)]
    pub game: GameCfg,
    #[serde(default)]
    pub database: DatabaseCfg,
    #[serde(default)]
    pub chat: ChatCfg,
    #[serde(default)]
    pub debug: DebugCfg,
    #[serde(default)]
    pub splits: SplitsCfg,
    #[serde(default)]
    pub attempts_counter: CounterCfg,
    /// LiveSplit's "Sum of Best Segments" row (seasonal, given his splits-file practice).
    #[serde(default)]
    pub lifetime_sob: CounterCfg,
    /// Alternate on-screen layouts (other OBS scenes). The base sections above
    /// are layout 0; the bot probes every layout's timer until one parses
    /// consistently, locks to it, and re-probes if the timer goes dark.
    #[serde(default)]
    pub layouts: Vec<LayoutCfg>,
    #[serde(default)]
    pub layout_search: LayoutSearchCfg,
}

/// Tolerance for the streamer nudging the LiveSplit window a few pixels:
/// when the locked timer goes dark, nearby pixel offsets are probed and, if
/// one parses consistently, the whole layout is re-anchored at that offset.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LayoutSearchCfg {
    /// Maximum offset searched in each direction (0 disables the search).
    pub drift_px: u32,
    /// Step between probed offsets.
    pub step_px: u32,
    /// Dark frames on the locked layout before probing resumes (every layout
    /// at its configured position, plus nearby offsets).
    pub dark_frames_search: u32,
}

impl Default for LayoutSearchCfg {
    fn default() -> Self {
        Self {
            drift_px: 36,
            step_px: 12,
            dark_frames_search: 30,
        }
    }
}

/// A crop rectangle in canvas coordinates.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
}

/// An alternate layout: same thresholds/upscale as the base sections, only
/// the rectangles move. Regions left out inherit the base rectangle.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutCfg {
    pub name: String,
    pub timer: Rect,
    #[serde(default)]
    pub splits: Option<Rect>,
    #[serde(default)]
    pub attempts_counter: Option<Rect>,
    #[serde(default)]
    pub lifetime_sob: Option<Rect>,
}

/// LiveSplit's lifetime attempt counter (the number in the layout header).
/// Read once per run and stored on the run row — correlates our per-category
/// numbering with the runner's own lifetime count.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CounterCfg {
    pub enabled: bool,
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    pub threshold: u8,
    pub invert: bool,
}

impl Default for CounterCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            crop_x: 0,
            crop_y: 0,
            crop_w: 120,
            crop_h: 34,
            threshold: 150,
            invert: true,
        }
    }
}

/// Second OCR region: the splits panel's cumulative-time column, one act per
/// row (rows sliced evenly). Detected per-act splits enable golds and pace.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SplitsCfg {
    pub enabled: bool,
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    /// How often to OCR the panel (splits change at most once per act).
    pub read_every_secs: u64,
    /// Threshold for the panel text. The cumulative column is always white,
    /// so this can sit high enough (default 150) to also read the row
    /// LiveSplit highlights with a colored background — crucial, because the
    /// first act's row is highlighted from the moment the run starts.
    pub threshold: u8,
    pub invert: bool,
    /// Values within this of the baseline count as "unchanged".
    pub tolerance_ms: i64,
    /// Consecutive reads a changed value needs before it's recorded.
    pub confirmations: usize,
}

impl Default for SplitsCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            crop_x: 0,
            crop_y: 0,
            crop_w: 135,
            crop_h: 288,
            read_every_secs: 5,
            threshold: 150,
            invert: true,
            // The panel displays tenths, so a real change is >= 100ms; 50
            // keeps every genuine change visible while absorbing OCR jitter.
            tolerance_ms: 50,
            confirmations: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DebugCfg {
    /// Append one JSON line per analyzed frame (OCR text, parsed value,
    /// phase, events) to this file. Invaluable for tuning detection against
    /// a VOD; replayable and `tail -f`-able.
    pub obs_log: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamCfg {
    /// Twitch channel login name (as in twitch.tv/<channel>).
    pub channel: String,
    /// "best", "worst", or a rendition name like "720p60".
    #[serde(default = "d_quality")]
    pub quality: String,
    #[serde(default)]
    pub source: SourceKind,
    /// Frames per second fed to OCR. Detection defaults assume 1.
    #[serde(default = "d_fps")]
    pub fps: u32,
    /// The stream is scaled to this canvas before cropping, so crop
    /// coordinates stay valid regardless of the delivered resolution.
    #[serde(default = "d_canvas_w")]
    pub canvas_w: u32,
    #[serde(default = "d_canvas_h")]
    pub canvas_h: u32,
    /// How often to re-check a channel that is offline (inside active_hours).
    #[serde(default = "d_offline_poll")]
    pub offline_poll_secs: u64,
    /// Local-time window when the streamer is expected, e.g. ["09:50", "20:30"].
    /// Outside it the bot polls only every `quiet_poll_secs` — graceful on
    /// Twitch and on the logs. Empty = always use offline_poll_secs.
    #[serde(default)]
    pub active_hours: Vec<String>,
    #[serde(default = "d_quiet_poll")]
    pub quiet_poll_secs: u64,
    /// Delay before restarting a pipeline that died mid-stream.
    #[serde(default = "d_restart_delay")]
    pub restart_delay_secs: u64,
    /// No frame for this long -> assume the pipeline hung and restart it.
    #[serde(default = "d_frame_timeout")]
    pub frame_timeout_secs: u64,
    /// Extra args passed to streamlink when source = "streamlink".
    #[serde(default = "d_sl_args")]
    pub streamlink_extra_args: Vec<String>,
    /// Twitch VOD id (the number in twitch.tv/videos/<id>) for source = "vod".
    #[serde(default)]
    pub vod_id: String,
    /// Local path or ffmpeg-readable URL for source = "file".
    #[serde(default)]
    pub input: String,
    /// Seconds into a recording (source = "vod"/"file") to start at.
    #[serde(default)]
    pub start_secs: f64,
    /// Only capture when the live broadcast title contains this string
    /// (case-insensitive) — the streamer's own labeling separates e.g.
    /// "Ninja Gaiden speedruns" days from other games. Empty/absent = always
    /// capture. While live-but-mismatched, the bot stays dormant and keeps
    /// polling, so it catches a mid-day switch to the tracked game.
    #[serde(default)]
    pub title_filter: Option<String>,
    /// Free-form tag stored on the session (e.g. "arcathlon") so the site can
    /// mark runs from special broadcasts.
    #[serde(default)]
    pub session_tag: Option<String>,
    /// When the recording started, RFC3339 (e.g. "2026-08-25T16:59:36Z").
    /// Recorded runs are then logged on the original broadcast timeline
    /// instead of the analysis time. For source = "vod" this is fetched from
    /// Twitch automatically when unset.
    #[serde(default)]
    pub recorded_start: Option<String>,
}

impl StreamCfg {
    /// Parsed active window as minutes-of-day (start, end), if configured.
    pub fn active_window(&self) -> Result<Option<(u32, u32)>> {
        if self.active_hours.is_empty() {
            return Ok(None);
        }
        if self.active_hours.len() != 2 {
            bail!("stream.active_hours must be [\"HH:MM\", \"HH:MM\"]");
        }
        let parse = |s: &str| -> Result<u32> {
            let (h, m) = s
                .split_once(':')
                .with_context(|| format!("bad time {s:?} in stream.active_hours"))?;
            let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
            if h > 23 || m > 59 {
                bail!("bad time {s:?} in stream.active_hours");
            }
            Ok(h * 60 + m)
        };
        Ok(Some((
            parse(&self.active_hours[0])?,
            parse(&self.active_hours[1])?,
        )))
    }

    pub fn recorded_start_ms(&self) -> Result<Option<i64>> {
        match &self.recorded_start {
            None => Ok(None),
            Some(s) => Ok(Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .with_context(|| format!("stream.recorded_start {s:?} is not RFC3339"))?
                    .timestamp_millis(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Resolve the HLS playlist URL ourselves and let ffmpeg read it directly.
    /// No tools beyond ffmpeg required.
    #[default]
    Hls,
    /// Pipe `streamlink --stdout` into ffmpeg (requires streamlink installed).
    Streamlink,
    /// A Twitch VOD (set stream.vod_id). Processes faster than realtime with
    /// deterministic frame-index timing — ideal for testing detection.
    Vod,
    /// A local video file or any ffmpeg-readable URL (set stream.input).
    /// Same deterministic timing as Vod.
    File,
}

impl SourceKind {
    /// Recorded sources tick the state machine by frame index, not wall
    /// clock, so processing speed doesn't affect detection.
    pub fn is_recorded(&self) -> bool {
        matches!(self, SourceKind::Vod | SourceKind::File)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerCfg {
    /// Crop rectangle for the on-screen timer, in canvas coordinates.
    /// Tune with `ngtwitchtimer calibrate` (and `--full-frame` to scout).
    #[serde(default)]
    pub crop_x: u32,
    #[serde(default)]
    pub crop_y: u32,
    #[serde(default = "d_crop_w")]
    pub crop_w: u32,
    #[serde(default = "d_crop_h")]
    pub crop_h: u32,
    /// Upscale factor before thresholding (3-4x helps tesseract a lot).
    #[serde(default = "d_upscale")]
    pub upscale: u32,
    /// Gray level above which a pixel counts as lit (0-255).
    #[serde(default = "d_threshold")]
    pub threshold: u8,
    /// true when the timer is light text on a dark background (the usual
    /// LiveSplit look); produces black digits on white for tesseract.
    #[serde(default = "d_true")]
    pub invert: bool,
}

impl Default for TimerCfg {
    fn default() -> Self {
        Self {
            crop_x: 0,
            crop_y: 0,
            crop_w: d_crop_w(),
            crop_h: d_crop_h(),
            upscale: d_upscale(),
            threshold: d_threshold(),
            invert: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrCfg {
    /// "auto" (leptess if compiled in, else cli), "cli", or "leptess".
    #[serde(default = "d_engine")]
    pub engine: String,
    #[serde(default = "d_lang")]
    pub lang: String,
    /// Binary to invoke for the cli engine.
    #[serde(default = "d_tess_cmd")]
    pub tesseract_cmd: String,
    /// Tessdata directory for the leptess engine (None = system default).
    #[serde(default)]
    #[cfg_attr(not(feature = "leptess-ocr"), allow(dead_code))]
    pub tessdata_path: Option<String>,
}

impl Default for OcrCfg {
    fn default() -> Self {
        Self {
            engine: d_engine(),
            lang: d_lang(),
            tesseract_cmd: d_tess_cmd(),
            tessdata_path: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameCfg {
    #[serde(default = "d_game")]
    pub name: String,
    #[serde(default = "d_category")]
    pub category: String,
    /// Acts/segments with their *cumulative* end times, used to bucket where
    /// resets happened (death chart). Give the final act no end_ms. Boundaries
    /// are approximate — pad them a bit above typical split times.
    #[serde(default)]
    pub acts: Vec<ActCfg>,
    /// What the best tracked run is called in announcements and reports.
    /// The tracked best only spans what the bot has seen, so "season best"
    /// is often more honest than the default "PB".
    #[serde(default = "d_record_label")]
    pub record_label: String,
    /// External reference times (world record, lifetime PB, ...) shown on
    /// the records site. `time` accepts the same formats as the timer.
    #[serde(default)]
    pub references: Vec<RefTime>,
    /// The best known time from BEFORE tracking started (e.g. read off the
    /// LiveSplit comparison column). A finish only counts as a NEW record if
    /// it beats this too, so the bot never claims a record it can't back.
    #[serde(default)]
    pub baseline_best: Option<String>,
    /// Only record runs while the layout's own title row names this game.
    /// Lets a marathon broadcast be captured, recording just the segments
    /// that are actually this game. Off by default: a misread title would
    /// otherwise stop recording.
    #[serde(default)]
    pub require_title_match: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefTime {
    pub label: String,
    pub time: String,
}

impl RefTime {
    pub fn ms(&self) -> Option<i64> {
        crate::timeparse::parse_time(&self.time)
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActCfg {
    pub name: String,
    #[serde(default)]
    pub end_ms: Option<i64>,
}

impl GameCfg {
    pub fn act_list(&self) -> Vec<(String, Option<i64>)> {
        self.acts
            .iter()
            .map(|a| (a.name.clone(), a.end_ms))
            .collect()
    }

    pub fn baseline_best_ms(&self) -> Option<i64> {
        self.baseline_best
            .as_deref()
            .and_then(crate::timeparse::parse_time)
    }
}

impl Default for GameCfg {
    fn default() -> Self {
        Self {
            name: d_game(),
            category: d_category(),
            acts: Vec::new(),
            record_label: d_record_label(),
            references: Vec::new(),
            baseline_best: None,
            require_title_match: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseCfg {
    #[serde(default = "d_db_path")]
    pub path: String,
}

impl Default for DatabaseCfg {
    fn default() -> Self {
        Self { path: d_db_path() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChatCfg {
    pub enabled: bool,
    /// Channel to join for announcements/commands. Empty = the watched
    /// stream.channel. Point this at YOUR OWN channel to test the bot
    /// without posting in the streamer's chat.
    pub channel: String,
    /// The bot account's login name.
    pub username: String,
    /// IRC OAuth token, with or without the "oauth:" prefix.
    pub oauth_token: String,
    /// Announce finished runs in chat.
    #[serde(default = "d_true")]
    pub announce: bool,
    #[serde(default = "d_cooldown")]
    pub command_cooldown_secs: u64,
    /// Extra logins allowed to use mod commands (besides badge-carrying
    /// moderators and the broadcaster).
    pub mods: Vec<String>,
    /// Namespace for commands so this bot never collides with other bots in
    /// a shared channel: with prefix "ng", commands are !ngpb, !ngdeaths,
    /// !ngpace, ... and the bare !pb is ignored. Empty = classic bare
    /// commands.
    pub command_prefix: String,
}

impl Config {
    /// A default config with one knob set, for tests that exercise logic
    /// depending on the category's fastest plausible time.
    #[cfg(test)]
    pub fn for_test_with_min_final(min_final_ms: i64) -> Self {
        let mut cfg: Config = toml::from_str("[stream]\nchannel = \"test\"\n")
            .expect("the minimal config must parse");
        cfg.detection.min_final_ms = min_final_ms;
        cfg
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        cfg.stream.channel = cfg
            .stream
            .channel
            .trim()
            .trim_start_matches('#')
            .to_ascii_lowercase();
        cfg.chat.mods = cfg
            .chat
            .mods
            .iter()
            .map(|m| m.to_ascii_lowercase())
            .collect();
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.stream.channel.is_empty() {
            bail!("stream.channel must be set");
        }
        if self.stream.fps == 0 {
            bail!("stream.fps must be >= 1");
        }
        let t = &self.timer;
        if t.crop_w == 0 || t.crop_h == 0 {
            bail!("timer.crop_w and timer.crop_h must be nonzero");
        }
        if t.crop_x + t.crop_w > self.stream.canvas_w || t.crop_y + t.crop_h > self.stream.canvas_h
        {
            bail!(
                "timer crop rectangle {}x{}+{}+{} falls outside the {}x{} canvas",
                t.crop_w,
                t.crop_h,
                t.crop_x,
                t.crop_y,
                self.stream.canvas_w,
                self.stream.canvas_h
            );
        }
        if !(1..=8).contains(&t.upscale) {
            bail!("timer.upscale must be between 1 and 8");
        }
        match self.ocr.engine.as_str() {
            "auto" | "cli" | "leptess" => {}
            other => bail!("ocr.engine must be \"auto\", \"cli\" or \"leptess\", got {other:?}"),
        }
        if self.chat.enabled && (self.chat.username.is_empty() || self.chat.oauth_token.is_empty())
        {
            bail!("chat.enabled = true requires chat.username and chat.oauth_token");
        }
        if self.stream.source == SourceKind::Vod && self.stream.vod_id.trim().is_empty() {
            bail!("stream.source = \"vod\" requires stream.vod_id");
        }
        if self.stream.source == SourceKind::File && self.stream.input.trim().is_empty() {
            bail!("stream.source = \"file\" requires stream.input");
        }
        self.stream.recorded_start_ms()?;
        self.stream.active_window()?;
        for l in &self.layouts {
            for (what, r) in [
                ("timer", Some(l.timer)),
                ("splits", l.splits),
                ("attempts_counter", l.attempts_counter),
                ("lifetime_sob", l.lifetime_sob),
            ] {
                if let Some(r) = r {
                    if r.crop_w == 0
                        || r.crop_h == 0
                        || r.crop_x + r.crop_w > self.stream.canvas_w
                        || r.crop_y + r.crop_h > self.stream.canvas_h
                    {
                        bail!(
                            "layout {:?} {what} rectangle is invalid for the canvas",
                            l.name
                        );
                    }
                }
            }
        }
        for r in &self.game.references {
            if r.ms().is_none() {
                bail!(
                    "game.references entry {:?} has unparseable time {:?}",
                    r.label,
                    r.time
                );
            }
        }
        if self.game.baseline_best.is_some() && self.game.baseline_best_ms().is_none() {
            bail!(
                "game.baseline_best {:?} is unparseable",
                self.game.baseline_best
            );
        }
        for (name, c) in [
            ("attempts_counter", &self.attempts_counter),
            ("lifetime_sob", &self.lifetime_sob),
        ] {
            if c.enabled
                && (c.crop_w == 0
                    || c.crop_h == 0
                    || c.crop_x + c.crop_w > self.stream.canvas_w
                    || c.crop_y + c.crop_h > self.stream.canvas_h)
            {
                bail!("{name} crop rectangle is invalid for the canvas");
            }
        }
        if self.splits.enabled {
            let s = &self.splits;
            if self.game.acts.is_empty() {
                bail!("splits.enabled requires [game] acts to be configured (one per panel row)");
            }
            if s.crop_w == 0 || s.crop_h == 0 {
                bail!("splits.crop_w and splits.crop_h must be nonzero");
            }
            if s.crop_x + s.crop_w > self.stream.canvas_w
                || s.crop_y + s.crop_h > self.stream.canvas_h
            {
                bail!("splits crop rectangle falls outside the canvas");
            }
        }
        Ok(())
    }
}

fn d_quality() -> String {
    "best".into()
}
fn d_fps() -> u32 {
    1
}
fn d_canvas_w() -> u32 {
    1920
}
fn d_canvas_h() -> u32 {
    1080
}
fn d_offline_poll() -> u64 {
    120
}
fn d_restart_delay() -> u64 {
    5
}
fn d_quiet_poll() -> u64 {
    1800
}
fn d_frame_timeout() -> u64 {
    30
}
fn d_sl_args() -> Vec<String> {
    vec!["--twitch-disable-ads".into()]
}
fn d_crop_w() -> u32 {
    320
}
fn d_crop_h() -> u32 {
    120
}
fn d_upscale() -> u32 {
    4
}
fn d_threshold() -> u8 {
    140
}
fn d_true() -> bool {
    true
}
fn d_engine() -> String {
    "auto".into()
}
fn d_lang() -> String {
    "eng".into()
}
fn d_tess_cmd() -> String {
    "tesseract".into()
}
fn d_game() -> String {
    "Unknown Game".into()
}
fn d_category() -> String {
    "Any%".into()
}
fn d_db_path() -> String {
    "ngtimer.db".into()
}
fn d_cooldown() -> u64 {
    10
}
fn d_record_label() -> String {
    "PB".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(s)?;
        cfg.stream.channel = cfg.stream.channel.trim().to_ascii_lowercase();
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let cfg = parse("[stream]\nchannel = \"SomeStreamer\"\n").unwrap();
        assert_eq!(cfg.stream.channel, "somestreamer");
        assert_eq!(cfg.stream.quality, "best");
        assert_eq!(cfg.stream.source, SourceKind::Hls);
        assert_eq!(cfg.timer.upscale, 4);
        assert!(cfg.timer.invert);
        assert_eq!(cfg.detection.stall_confirmations, 5);
        assert_eq!(cfg.game.category, "Any%");
        assert!(!cfg.chat.enabled);
    }

    #[test]
    fn rejects_out_of_canvas_crop() {
        let err =
            parse("[stream]\nchannel = \"x\"\n[timer]\ncrop_x = 1800\ncrop_w = 200\n").unwrap_err();
        assert!(err.to_string().contains("canvas"));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(parse("[stream]\nchannel = \"x\"\nchanel_typo = 1\n").is_err());
    }

    #[test]
    fn chat_requires_credentials() {
        let err = parse("[stream]\nchannel = \"x\"\n[chat]\nenabled = true\n").unwrap_err();
        assert!(err.to_string().contains("oauth_token"));
    }

    #[test]
    fn detection_overrides_apply() {
        let cfg =
            parse("[stream]\nchannel = \"x\"\n[detection]\nstall_confirmations = 9\n").unwrap();
        assert_eq!(cfg.detection.stall_confirmations, 9);
        // untouched fields keep defaults
        assert_eq!(cfg.detection.start_confirmations, 3);
    }
}
